export type EpisodeDecision = 'included' | 'excluded' | 'needs_review';

export interface ReviewContext {
  type: 'kratos:review-context';
  versionId: string;
  idToken: string;
  apiBase?: string;
  decisions?: Record<string, EpisodeDecision>;
  source?: {
    kind: 'uploaded';
    label: string;
    revision: string;
    info: DatasetInfo;
    fileBaseUrl: string;
  };
}

function isReviewContext(value: unknown): value is ReviewContext {
  if (!value || typeof value !== 'object') return false;
  const candidate = value as Partial<ReviewContext>;
  return candidate.type === 'kratos:review-context'
    && typeof candidate.versionId === 'string'
    && candidate.versionId.length > 0
    && typeof candidate.idToken === 'string'
    && candidate.idToken.length > 0;
}

export function listenForReviewContext(onContext: (context: ReviewContext) => void): void {
  window.addEventListener('message', (event: MessageEvent<unknown>) => {
    if (event.origin !== window.location.origin || !isReviewContext(event.data)) return;
    onContext(event.data);
  });

  if (window.parent !== window) {
    window.parent.postMessage({ type: 'kratos:review-ready' }, window.location.origin);
  }
}

export async function saveEpisodeDecision(
  context: ReviewContext,
  episodeIndex: number,
  decision: EpisodeDecision,
  note?: string,
): Promise<void> {
  const apiBase = (context.apiBase ?? '').replace(/\/$/, '');
  const response = await fetch(
    `${apiBase}/api/v1/operator/dataset-versions/${encodeURIComponent(context.versionId)}/episodes/${episodeIndex}/curation`,
    {
      method: 'PUT',
      headers: {
        Authorization: `Bearer ${context.idToken}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({ decision, note: note?.trim() || null }),
    },
  );

  if (!response.ok) {
    const body = await response.text();
    throw new Error(body || `Kratos returned ${response.status}`);
  }
}
import type { DatasetInfo } from '../types';
