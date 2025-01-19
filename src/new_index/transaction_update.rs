use bitcoin::Txid;
use std::collections::HashSet;

#[derive(Debug, Default, Clone)]
pub struct TransactionUpdate {
    /// The set of transactions that entered (were added to) the mempool in this update
    mempool_added: HashSet<Txid>,

    /// The set of transactions that left (were removed from) the mempool in this update
    mempool_removed: HashSet<Txid>,

    /// The set of transactions that were added to (mined in) the current best chain
    block_added: HashSet<Txid>,

    /// The set of transactions that were removed from the best chain (reorged out)
    block_removed: HashSet<Txid>,
}

/// This struct is returned by `categorize()` to hold
/// each distinct category of transaction transitions.
#[derive(Debug, Default, Clone)]
pub struct CategorizedTxs {
    /// Transactions that were *both* in mempool_removed **and** in block_added.
    /// Typically these were mined out of the mempool in the new block.
    pub mined_from_mempool: HashSet<Txid>,

    /// Transactions that were *both* block_removed **and** mempool_added.
    /// Typically these were reorged out, but showed up again in mempool.
    pub reorged_back_to_mempool: HashSet<Txid>,

    /// Transactions that were *only* in mempool_added (and not block_removed, etc.).
    /// These are newly seen in the mempool for the first time.
    pub new_in_mempool_only: HashSet<Txid>,

    /// Transactions that were *only* in block_added (and not mempool_removed),
    /// i.e. we never saw them in mempool first.
    pub new_block_only: HashSet<Txid>,

    /// Transactions that were in block_removed but *not* re-mined
    /// and *not* re‐added to mempool.  
    /// (They have effectively “disappeared” from the best chain, and are not in mempool.)
    pub reorged_out_entirely: HashSet<Txid>,

    /// Transactions that were mempool_removed but *not* found in block_added,
    /// so presumably RBF or evicted (or conflicted, or pruned).
    pub mempool_rbf_or_evicted: HashSet<Txid>,

    /// Transactions that appear in *both* `block_removed` and `block_added`.
    /// That can happen in a reorg where a transaction was removed with the old block
    /// but is also included in the new chain tip block. (Hence it never hits the mempool.)
    pub reorged_out_and_remined: HashSet<Txid>,
}

pub struct TransactionChangeSet {
    pub removed: HashSet<Txid>,
    pub added: HashSet<Txid>,
}

impl TransactionUpdate {
    pub fn new(mempool: TransactionChangeSet, block: TransactionChangeSet) -> Self {
        Self {
            mempool_added: mempool.added,
            mempool_removed: mempool.removed,
            block_added: block.added,
            block_removed: block.removed,
        }
    }

    pub fn update_block(&mut self, block: TransactionChangeSet) {
        self.block_added = block.added;
        self.block_removed = block.removed;
    }

    pub fn update_mempool(&mut self, mempool: TransactionChangeSet) {
        self.mempool_added = mempool.added;
        self.mempool_removed = mempool.removed;
    }

    pub fn reset(&mut self) {
        self.mempool_added = HashSet::new();
        self.mempool_removed = HashSet::new();
        self.block_added = HashSet::new();
        self.block_removed = HashSet::new();
    }

    /// Categorize transactions into buckets describing what happened to them
    /// across mempool and block changes.
    pub fn categorize(&self) -> CategorizedTxs {
        let mut result = CategorizedTxs::default();

        // 1. Mined from mempool: present in both mempool_removed and block_added
        for txid in self.mempool_removed.intersection(&self.block_added) {
            result.mined_from_mempool.insert(*txid);
        }

        // 2. Reorged back to mempool: present in both block_removed and mempool_added
        for txid in self.block_removed.intersection(&self.mempool_added) {
            result.reorged_back_to_mempool.insert(*txid);
        }

        // 3. Newly in mempool only (i.e. in mempool_added but *not* also in block_removed)
        //    and not in the intersection from #2
        for txid in &self.mempool_added {
            if !self.block_removed.contains(txid) {
                result.new_in_mempool_only.insert(*txid);
            }
        }
        // You might prefer a set-difference approach:
        //   let new_in_mempool = &self.mempool_added - &self.block_removed;
        //   result.new_in_mempool_only = new_in_mempool - &result.reorged_back_to_mempool;

        // 4. New in block only (i.e. in block_added but not also in mempool_removed)
        //    ignoring the intersection from #1
        for txid in &self.block_added {
            if !self.mempool_removed.contains(txid) {
                result.new_block_only.insert(*txid);
            }
        }

        // 5. Reorged out entirely:
        //    in block_removed, but not reorged_back_to_mempool, not re-mined
        //    So: block_removed ∖ (mempool_added ∪ block_added).
        for txid in &self.block_removed {
            let is_reorged_back = self.mempool_added.contains(txid);
            let is_remined = self.block_added.contains(txid);
            if !is_reorged_back && !is_remined {
                result.reorged_out_entirely.insert(*txid);
            }
        }

        // 6. Mempool RBF or eviction:
        //    in mempool_removed, but *not* in block_added (so not mined).
        //    This lumps all mempool removals that aren't a direct “mined” event.
        //    Some of these might be genuine RBF replaced, some might be eviction, conflict, etc.
        for txid in &self.mempool_removed {
            if !self.block_added.contains(txid) {
                result.mempool_rbf_or_evicted.insert(*txid);
            }
        }

        // 7. Reorged out and re-mined:
        //    in block_removed as well as block_added. That means it was removed
        //    (the old block got replaced) but also added in the new chain tip block.
        //    No time in mempool. It's fairly unusual but can happen.
        for txid in self.block_removed.intersection(&self.block_added) {
            result.reorged_out_and_remined.insert(*txid);
        }

        result
    }

    /// Categorize into exactly two “buckets”:
    ///
    /// 1. **Newly added**:
    ///    TXs that were **not** previously in mempool or chain,
    ///    but are **now** in mempool or chain.
    ///
    /// 2. **Fully removed**:
    ///    TXs that **were** in mempool or chain,
    ///    but are now in **neither**.
    ///
    /// Everything else (e.g. reorg from chain to mempool, or mempool to chain)
    /// does *not* appear in these two sets.
    ///
    /// Returns `(newly_added, fully_removed)`.
    pub fn categorize_into_two(&self) -> TransactionChangeSet {
        // We consider the union of `mempool_added` and `block_added` to be
        // “transactions that ended up in the system (mempool or chain) *now*”.
        //
        // We consider the union of `mempool_removed` and `block_removed` to be
        // “transactions that left the system *now*”.
        //
        // However, we only want:
        //   - newly_added: those that are *only* in the added sets and *not*
        //                  in the removed sets (so they truly came from outside).
        //
        //   - fully_removed: those that are *only* in the removed sets
        //                    and *not* in the added sets (so they truly left entirely).
        //
        // If a transaction is both in `mempool_added` and `mempool_removed`,
        // that implies it was in the mempool before, left, and re-entered in the same update,
        // so it is *not* “brand-new from outside.” Likewise for `block_added` + `block_removed`.

        let in_added = &self.mempool_added | &self.block_added; // union
        let in_removed = &self.mempool_removed | &self.block_removed; // union

        // 1. Newly added: present in `in_added` but NOT present in `in_removed`.
        //    This means they were not in our system *before* this update,
        //    but have arrived in mempool or chain now.
        let newly_added = &in_added - &in_removed;

        // 2. Fully removed: present in `in_removed` but NOT present in `in_added`.
        //    This means they were in our system *before* (mempool or chain),
        //    but they are absent now.
        let fully_removed = &in_removed - &in_added;

        TransactionChangeSet {
            removed: fully_removed,
            added: newly_added,
        }
    }
}
