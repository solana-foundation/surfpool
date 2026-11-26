# Changelog

## [1.8.0](https://github.com/solana-foundation/surfpool/compare/v1.7.0...v1.8.0) (2026-11-25)


### Features

* add opt-in strict snapshot loading ([#905](https://github.com/solana-foundation/surfpool/issues/905)) ([d9fd2a5](https://github.com/solana-foundation/surfpool/commit/d9fd2a58a024317c89b886ce1e7860fd44bfefba))
* **core/scenarios:** introduce keep alive feature ([#906](https://github.com/solana-foundation/surfpool/issues/906)) ([2a01556](https://github.com/solana-foundation/surfpool/commit/2a01556113ef241a5eaae06b4d8f6bb4e4d92904))
* make surfpool harbor friendly ([#902](https://github.com/solana-foundation/surfpool/issues/902)) ([d445838](https://github.com/solana-foundation/surfpool/commit/d4458382f900a075182bc58d5b20bfd3a4dc0843))
* **scenarios:** add BisonFi support - state templates & presets ([#948](https://github.com/solana-foundation/surfpool/issues/948)) ([0334564](https://github.com/solana-foundation/surfpool/commit/0334564083f33802fd39eafee33302e5c81eb239))
* **scenarios:** add GoonFi support - state templates & presets ([#965](https://github.com/solana-foundation/surfpool/issues/965)) ([3090142](https://github.com/solana-foundation/surfpool/commit/309014210db043b30b31f859b468ae4ef3fd60d8))
* **scenarios:** add HumidiFi support - state templates & presets ([#954](https://github.com/solana-foundation/surfpool/issues/954)) ([8dcc1b3](https://github.com/solana-foundation/surfpool/commit/8dcc1b3d06a6c01417bb19226e013aa231ddd0bd))
* **scenarios:** add SolFi support - state templates & presets ([#949](https://github.com/solana-foundation/surfpool/issues/949)) ([28d3671](https://github.com/solana-foundation/surfpool/commit/28d36714555846dcc614d52afc69fcabe8380fc4))
* **scenarios:** prepare Phoenix Eternal state through Phoenix's own instructions ([#862](https://github.com/solana-foundation/surfpool/issues/862)) ([ec27259](https://github.com/solana-foundation/surfpool/commit/ec2725961396db44b94e36babe5f4a11e317e5cf))
* stream the Geyser slot lifecycle and implement blockSubscribe ([#890](https://github.com/solana-foundation/surfpool/issues/890)) ([02c4d74](https://github.com/solana-foundation/surfpool/commit/02c4d74425d04c0fb9fae5a913232665427af791))


### Bug Fixes

* **cli:** handle startup cancellation and bound snapshot event draining ([#878](https://github.com/solana-foundation/surfpool/issues/878)) ([253f314](https://github.com/solana-foundation/surfpool/commit/253f3143344af8d71c1ccc3d87fd481b1e1b6003))
* **core:** back Token-2022 wrapped SOL with lamports in setTokenAccount ([#931](https://github.com/solana-foundation/surfpool/issues/931)) ([a4ccf4b](https://github.com/solana-foundation/surfpool/commit/a4ccf4bba1661444ae53053329e9050bcbdc2a4e))
* **core:** clarify errors for managed sysvar updates ([#899](https://github.com/solana-foundation/surfpool/issues/899)) ([6acd394](https://github.com/solana-foundation/surfpool/commit/6acd394afea27e744643015b1bf247e9fe4b6a01))
* **core:** reject time travel arithmetic overflow ([#918](https://github.com/solana-foundation/surfpool/issues/918)) ([7832d6e](https://github.com/solana-foundation/surfpool/commit/7832d6e42de8e78a74a8ff6ef02400e3ee1e43a8))
* **core:** report the absolute slot in confirmed signature notifications ([#898](https://github.com/solana-foundation/surfpool/issues/898)) ([0c1827b](https://github.com/solana-foundation/surfpool/commit/0c1827b90f8db107e4bc37fa78b0b15d19aef58b))
* **core:** support Yellowstone gRPC v16 through the Agave 4.3 Geyser methods ([#910](https://github.com/solana-foundation/surfpool/issues/910)) ([7e020f7](https://github.com/solana-foundation/surfpool/commit/7e020f78f8ed95a69ad279a3ee200b28e6f3c2cd))
* **core:** unload plugins before dropping their libraries ([#932](https://github.com/solana-foundation/surfpool/issues/932)) ([89887c5](https://github.com/solana-foundation/surfpool/commit/89887c5e4f9bd97aeb83a6261fae6fe9682b1317))
* **deps:** resolve Dependabot alerts ([#926](https://github.com/solana-foundation/surfpool/issues/926)) ([b878d33](https://github.com/solana-foundation/surfpool/commit/b878d33ec491bbb04ae0646f1c1599655510dff2))
* **mcp:** reject overflowing token balance amounts ([#919](https://github.com/solana-foundation/surfpool/issues/919)) ([e9fb8ff](https://github.com/solana-foundation/surfpool/commit/e9fb8ff46d4aa2b98bf599abfba56aae490fd17c))
* **rpc:** answer resends, getSignaturesForAddress, getBlockCommitment and token balances as Agave does ([#888](https://github.com/solana-foundation/surfpool/issues/888)) ([81b4e92](https://github.com/solana-foundation/surfpool/commit/81b4e92b7d17bb0ec406c6e1a31692c8b0b02b5d))
* **rpc:** sanitize transactions before reading them, as Agave does ([#911](https://github.com/solana-foundation/surfpool/issues/911)) ([a2b83fb](https://github.com/solana-foundation/surfpool/commit/a2b83fb4154cd65c4c30414ea1a47faae86172a3))
* **scenarios:** apply overrides only when the surfnet produces their slot ([#925](https://github.com/solana-foundation/surfpool/issues/925)) ([f103d91](https://github.com/solana-foundation/surfpool/commit/f103d91cc587bb1aff63de511e626f7841b54243))
* **scenarios:** follow Phoenix's deployed program - funding fields, error code and published IDL ([#968](https://github.com/solana-foundation/surfpool/issues/968)) ([9980395](https://github.com/solana-foundation/surfpool/commit/998039594f1b79be1344eba360cf387e651d3dad))
* **scenarios:** follow Pump's deployed programs - IDLs from pump-public-docs and the mayhem fee recipient ([#969](https://github.com/solana-foundation/surfpool/issues/969)) ([c148475](https://github.com/solana-foundation/surfpool/commit/c14847558f3ee6f8205d78e1a9409a7f0935e4bc))
* **scenarios:** refresh the Kamino IDLs to the deployed programs ([#967](https://github.com/solana-foundation/surfpool/issues/967)) ([fbc49a1](https://github.com/solana-foundation/surfpool/commit/fbc49a15cf284a154067c733d0611875cc89ba01))
* serve TPU QUIC so `solana program deploy` works with Agave 4.5 ([#971](https://github.com/solana-foundation/surfpool/issues/971)) ([f864286](https://github.com/solana-foundation/surfpool/commit/f864286cc80378dd64b32f4e4b6b7b96de625b9c))


### Performance Improvements

* **core:** near-free instruction profiling, cheaper sendTransaction, instant WebSocket notifications, smaller idle heap ([#907](https://github.com/solana-foundation/surfpool/issues/907)) ([d552d45](https://github.com/solana-foundation/surfpool/commit/d552d450707fdc38df5a737b2bbbbec2b397b771))
