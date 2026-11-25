window.BENCHMARK_DATA = {
  "lastUpdate": 1795615140832,
  "repoUrl": "https://github.com/solana-foundation/surfpool",
  "entries": {
    "surfpool throughput": [
      {
        "commit": {
          "author": {
            "email": "bakasura@abv.bg",
            "name": "bakasura980",
            "username": "bakasura980"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "309014210db043b30b31f859b468ae4ef3fd60d8",
          "message": "feat(scenarios): add GoonFi support - state templates & presets (#965)\n\nCo-authored-by: Georgi Petroff <102983346+92Infinitus92@users.noreply.github.com>",
          "timestamp": "2026-11-24T13:46:28-05:00",
          "tree_id": "bcc8f259ff6d156b5c42f9bad82e2bad7fdf3122",
          "url": "https://github.com/solana-foundation/surfpool/commit/309014210db043b30b31f859b468ae4ef3fd60d8"
        },
        "date": 1795546965243,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "gpa/rps",
            "value": 98,
            "range": "97.5..99.2",
            "unit": "req/s"
          },
          {
            "name": "reads/rps",
            "value": 63881.6,
            "range": "62037.1..64967.0",
            "unit": "req/s"
          },
          {
            "name": "send_tx_default/send_rate",
            "value": 2258.5,
            "range": "2257.4..2326.9",
            "unit": "tx/s"
          },
          {
            "name": "send_tx_default/tps_confirmed",
            "value": 2210.0623576868825,
            "range": "2197.5..2265.3",
            "unit": "tx/s"
          },
          {
            "name": "send_tx_no_profiling/send_rate",
            "value": 2294,
            "range": "2225.0..2368.1",
            "unit": "tx/s"
          },
          {
            "name": "send_tx_no_profiling/tps_confirmed",
            "value": 2233.511041957845,
            "range": "2177.2..2311.2",
            "unit": "tx/s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "102983346+92Infinitus92@users.noreply.github.com",
            "name": "Georgi Petroff",
            "username": "92Infinitus92"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "c14847558f3ee6f8205d78e1a9409a7f0935e4bc",
          "message": "fix(scenarios): follow Pump's deployed programs - IDLs from pump-public-docs and the mayhem fee recipient (#969)",
          "timestamp": "2026-11-25T08:54:04-05:00",
          "tree_id": "90faea668e36ecc2a9767524cadf46c8af58846c",
          "url": "https://github.com/solana-foundation/surfpool/commit/c14847558f3ee6f8205d78e1a9409a7f0935e4bc"
        },
        "date": 1795615139767,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "gpa/rps",
            "value": 101.9,
            "range": "101.2..102.3",
            "unit": "req/s"
          },
          {
            "name": "reads/rps",
            "value": 64235.4,
            "range": "61998.9..64528.9",
            "unit": "req/s"
          },
          {
            "name": "send_tx_default/send_rate",
            "value": 2310.8,
            "range": "2247.9..2372.2",
            "unit": "tx/s"
          },
          {
            "name": "send_tx_default/tps_confirmed",
            "value": 2252.676033314865,
            "range": "2188.3..2310.3",
            "unit": "tx/s"
          },
          {
            "name": "send_tx_no_profiling/send_rate",
            "value": 2318.9,
            "range": "2243.2..2379.3",
            "unit": "tx/s"
          },
          {
            "name": "send_tx_no_profiling/tps_confirmed",
            "value": 2268.4226965693606,
            "range": "2178.5..2311.0",
            "unit": "tx/s"
          }
        ]
      }
    ]
  }
}