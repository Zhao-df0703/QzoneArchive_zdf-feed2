const ARCHIVE_INTERVAL_KEY = "qzone-archive-page-interval";
const ARCHIVE_TARGET_YEAR_KEY = "qzone-archive-target-year";
export const MIN_ARCHIVE_INTERVAL = 2000;
export const DEFAULT_ARCHIVE_INTERVAL = 3000;
export const DEFAULT_ARCHIVE_TARGET_YEAR = 2017;

export function getArchiveInterval() {
  const value = Number(localStorage.getItem(ARCHIVE_INTERVAL_KEY));
  return Number.isFinite(value) ? Math.min(30000, Math.max(MIN_ARCHIVE_INTERVAL, Math.round(value))) : DEFAULT_ARCHIVE_INTERVAL;
}

export function setArchiveInterval(value: number) {
  const normalized = Math.min(30000, Math.max(MIN_ARCHIVE_INTERVAL, Math.round(value || DEFAULT_ARCHIVE_INTERVAL)));
  localStorage.setItem(ARCHIVE_INTERVAL_KEY, String(normalized));
  return normalized;
}

export function resetAppSettings() {
  localStorage.removeItem(ARCHIVE_INTERVAL_KEY);
  localStorage.removeItem(ARCHIVE_TARGET_YEAR_KEY);
}

export function getArchiveTargetYear() {
  const raw = localStorage.getItem(ARCHIVE_TARGET_YEAR_KEY);
  if (raw === null) return DEFAULT_ARCHIVE_TARGET_YEAR;
  const value = Number(raw);
  return Number.isFinite(value) ? Math.round(value) : DEFAULT_ARCHIVE_TARGET_YEAR;
}

export function setArchiveTargetYear(value: number) {
  const normalized = Math.round(value);
  localStorage.setItem(ARCHIVE_TARGET_YEAR_KEY, String(normalized));
  return normalized;
}
