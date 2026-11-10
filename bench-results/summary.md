## Surfpool benchmark: `head` vs `base`

✅ **No regression**. A gated metric fails when `head` is more than 15% worse than `base`. Values are medians over interleaved rounds, with the range in brackets.

| Scenario | Metric | base | head | Change | |
|---|---|---:|---:|---:|:--:|
| `send_tx_default` | `tps_confirmed` (tx/s) | 70.3 [70.0..70.6] | 1859.2 [1853.8..1899.7] | +2543.2% | ✅ |
| `send_tx_default` | `confirm_latency_p50_ms` (ms) | 366.8 [366.5..367.9] | 164.8 [164.0..168.8] | -55.1% | ℹ️ |
| `send_tx_default` | `send_latency_p50_ms` (ms) | 226.7 [222.2..227.6] | 8.1 [7.7..8.4] | -96.4% | ℹ️ |
| `send_tx_default` | `send_latency_p99_ms` (ms) | 252.3 [246.6..256.1] | 17.3 [14.5..18.9] | -93.1% | ℹ️ |
| `send_tx_default` | `send_rate` (tx/s) | 72.5 [71.8..73.6] | 1911.7 [1901.1..1945.8] | +2536.8% | ℹ️ |
| `send_tx_no_profiling` | `tps_confirmed` (tx/s) | 757.6 [751.5..772.7] | 1976.5 [1923.7..2054.5] | +160.9% | ✅ |
| `send_tx_no_profiling` | `confirm_latency_p50_ms` (ms) | 179.1 [176.9..183.3] | 163.6 [163.5..166.8] | -8.6% | ℹ️ |
| `send_tx_no_profiling` | `send_latency_p50_ms` (ms) | 19.0 [18.7..20.0] | 7.7 [7.4..8.0] | -59.5% | ℹ️ |
| `send_tx_no_profiling` | `send_latency_p99_ms` (ms) | 33.8 [33.1..40.4] | 14.2 [14.0..15.3] | -58.1% | ℹ️ |
| `send_tx_no_profiling` | `send_rate` (tx/s) | 777.0 [769.8..792.7] | 2027.4 [1972.6..2097.7] | +160.9% | ℹ️ |
| `reads` | `rps` (req/s) | 43453.1 [42746.2..44470.6] | 41782.9 [41309.5..43639.1] | -3.8% | ✅ |
| `reads` | `get_account_info_p50_ms` (ms) | 0.2 [0.2..0.2] | 0.3 [0.2..0.3] | +3.7% | ℹ️ |
| `reads` | `get_balance_p50_ms` (ms) | 0.2 [0.2..0.2] | 0.2 [0.2..0.2] | +4.0% | ℹ️ |
| `reads` | `get_multiple_accounts_p50_ms` (ms) | 0.7 [0.6..0.7] | 0.7 [0.7..0.7] | +2.8% | ℹ️ |
| `gpa` | `rps` (req/s) | 192.2 [179.7..199.0] | 175.2 [150.0..176.2] | -8.8% | ✅ |
| `gpa` | `latency_p50_ms` (ms) | 19.3 [17.5..20.2] | 20.8 [19.7..25.8] | +8.1% | ℹ️ |
| `gpa` | `latency_p99_ms` (ms) | 42.4 [41.5..42.6] | 46.8 [46.0..48.9] | +10.3% | ℹ️ |

✅ / ❌ gated metric · ℹ️ informational, never fails the run · ⚠️ missing on one side
