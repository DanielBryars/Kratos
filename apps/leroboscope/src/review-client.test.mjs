import assert from 'node:assert/strict';
import test from 'node:test';

import { isReviewContext, shouldWaitForReviewContext } from './kratos/review-client.ts';

const context = {
  type: 'kratos:review-context',
  versionId: 'version-1',
  idToken: 'identity-token',
  source: {
    kind: 'uploaded',
    label: 'SO-101 pick and place',
    revision: 'v1',
    info: { codebase_version: '3.0', features: {} },
    fileBaseUrl: '/api/v1/dataset-previews/preview-1/files/',
    previewToken: 'kpv_preview_secret',
  },
};

test('accepts the console dataset review handshake', () => {
  assert.equal(isReviewContext(context), true);
});

test('refuses the old incompatible preview message', () => {
  assert.equal(isReviewContext({
    kind: 'kratos.preview',
    version_id: 'version-1',
    files_base_url: '/files/',
    preview_token: 'secret',
  }), false);
});

test('requires a preview credential for an uploaded source', () => {
  const withoutCredential = structuredClone(context);
  delete withoutCredential.source.previewToken;
  assert.equal(isReviewContext(withoutCredential), false);
});

test('an embedded viewer waits for Kratos instead of loading the demo dataset', () => {
  assert.equal(shouldWaitForReviewContext(true, false), true);
  assert.equal(shouldWaitForReviewContext(false, false), false);
  assert.equal(shouldWaitForReviewContext(true, true), false);
});
