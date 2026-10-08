import { Store } from '../index.js';
// Explicit graph opt-in on the same Store used by document operations.
export function openGraph(path, options = {}) {
  const {autoReclaim, reclaimAfterBytes, ...open} = options;
  const s = new Store(path, {maxResidentBytes: 268435456n, ...open});
  try {
    if (!open.readOnly) {
      s.enableGraph();
      if (autoReclaim !== undefined || reclaimAfterBytes !== undefined) s.graphSetMaintenancePolicy({autoReclaim, reclaimAfterBytes});
    }
    return s;
  } catch (error) { s.close(); throw error; }
}
