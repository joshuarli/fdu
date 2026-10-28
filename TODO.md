Prefix: FDU
Next: 6

### FDU-4 · P3 · Browser and `--rmrf` each fold scan events

- **Area:** Architecture
- **Where:** `src/browser.rs:601`, `src/browser.rs:657`, `src/rmrf.rs:360`, `src/rmrf.rs:470`, `src/rmrf.rs:542`
- **Problem:** `BrowserModel::apply_entry_batch` and `rmrf::apply_batch` both append scanner batches and roll byte totals to ancestors. `build_index` says it folds events the same way the browser does. It does not. The browser stores `modified_time` with `set_modified_time`. `--rmrf` leaves every entry at `ModifiedTime::default`. The browser treats `NodeStatus::Scanning` and `NodeStatus::Stale` as ordinary entries. `--rmrf` marks the parent incomplete for those states. The indexed scanner currently publishes `Complete`, `Incomplete`, or `Excluded`, so the state split is latent. The next scan-event change has two writers and no check that the trees match.
- **Recommendation:** Fold `ScanEvent` into a `Tree` in one function, and call it from the browser and from `--rmrf`. Keep browser-only listing updates outside that function. Add one test that a batch containing a directory, a file, and an incomplete entry produces the same states, names, modified times, and ancestor totals in both callers.
- **Fix with:** single-source-of-truth
- **Effort:** M
