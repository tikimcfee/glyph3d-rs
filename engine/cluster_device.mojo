# cluster_device.mojo — facade for the sequence pass's device kernels.
#
# Split 2026-09 (the code-shape refactor): the tables live in
# cluster_tables.mojo, the probes in cluster_probe.mojo, the chain kernels
# in cluster_chain.mojo. This file re-exports all three so the consumers'
# from-imports keep resolving (verified: transitive from-imports work on the
# pinned compiler); it DIES when the sweep repoints gpu_pipeline.mojo and
# gpu_cluster.mojo directly. The original contract — one definition so the
# two harnesses can never drift the rule between them — is unchanged, just
# three files narrower each.

from cluster_tables import (
    KEY_CAP, HEAD_BMP_WORDS, BLOCK_LOG2, BLOCK,
    build_head_bitmap, build_state_table,
    ST_STRIDE, ST_EMPTY, st_mix, st_probe,
)
from cluster_probe import k_cluster_probe, k_decode_probe
from cluster_chain import (
    k_cluster_chain, chain_walk,
    CLIST_CAP, CLIST_STRIDE, CLIST_OVERFLOW,
    k_chain_free, block_eval, SB_BLOCKS,
    k_chain_sb_free, k_chain_sb_stitch, k_chain_sb_apply,
    k_chain_commit, k_chain_sb_cascade,
)
