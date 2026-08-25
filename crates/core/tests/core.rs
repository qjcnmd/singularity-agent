//! core 取消合同测试。

use singularity_core::CancellationToken;

#[test]
fn cloned_cancellation_tokens_share_one_monotonic_state() {
    let token = CancellationToken::new();
    let clone = token.clone();

    assert!(!token.is_cancelled());
    clone.cancel();
    assert!(token.is_cancelled());
}
