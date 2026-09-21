const overlay = () => document.getElementById('status-overlay')!;
const spinnerEl = () => overlay().querySelector('.spinner') as HTMLElement;
const statusTextEl = () => overlay().querySelector('.status-text') as HTMLElement;
const errorTextEl = () => overlay().querySelector('.error-text') as HTMLElement | null;

export function showLoading(message: string): void {
  const el = overlay();
  el.classList.remove('hidden');
  spinnerEl().style.display = 'block';

  // Remove old error text if any
  const errEl = errorTextEl();
  if (errEl) errEl.remove();

  statusTextEl().textContent = message;
}

export function showError(message: string): void {
  const el = overlay();
  el.classList.remove('hidden');
  spinnerEl().style.display = 'none';
  statusTextEl().textContent = '';

  // Create or update error text
  let errEl = errorTextEl();
  if (!errEl) {
    errEl = document.createElement('div');
    errEl.className = 'error-text';
    el.appendChild(errEl);
  }
  errEl.textContent = message;
}

export function hideStatus(): void {
  overlay().classList.add('hidden');
}
