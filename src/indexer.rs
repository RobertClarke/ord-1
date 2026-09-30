//! Orchestration: fresh index → fast sync; then the live loop (poll, reorg
//! handling, per-block commit); publishes the supply snapshot for the server.
//!
//! OWNER: store+server+indexer agent.
