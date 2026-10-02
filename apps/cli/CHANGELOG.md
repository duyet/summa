# Changelog

## [0.1.5](https://github.com/duyet/summa/compare/v0.1.4...v0.1.5) (2026-10-02)


### Features

* **cli:** import Command Code usage ([#141](https://github.com/duyet/summa/issues/141)) ([7226379](https://github.com/duyet/summa/commit/72263793b279069efb04f9bcf98177490fabb060))
* **cli:** import Devin CLI usage ([90f537f](https://github.com/duyet/summa/commit/90f537f661996369c22956e2a67102a46201deb3))
* **cli:** import pi and fx usage ([#138](https://github.com/duyet/summa/issues/138)) ([21f68dd](https://github.com/duyet/summa/commit/21f68dd2145c5ebb1b8b0cc8a73e5b251c51791f))


### Bug Fixes

* **cli:** add OpenAI pricing, which was missing entirely ([#142](https://github.com/duyet/summa/issues/142)) ([d1b03b2](https://github.com/duyet/summa/commit/d1b03b279537bdb312b9d511efe8030b385e1d28))
* **cli:** refresh Claude and Gemini rates to current list prices ([#139](https://github.com/duyet/summa/issues/139)) ([dd97d2a](https://github.com/duyet/summa/commit/dd97d2a5ee547f113a70bdc1eb3e4a3343588b54))
* **cli:** stop cost distribution producing negative rows ([#140](https://github.com/duyet/summa/issues/140)) ([349c9c7](https://github.com/duyet/summa/commit/349c9c7519e1b3a844762d5acd9ae8bf49f268a1))
* **cli:** stop pi being read twice ([#143](https://github.com/duyet/summa/issues/143)) ([7d3aa9f](https://github.com/duyet/summa/commit/7d3aa9fac33a6d7e64aef7e58c10ffe351229301))
* **cli:** surface hub-rejected rows and hidden sink errors in telemetry ([#145](https://github.com/duyet/summa/issues/145)) ([763adf5](https://github.com/duyet/summa/commit/763adf5b793c20a147f90824a2ce9bdd38b29238))

## [0.1.4](https://github.com/duyet/summa/compare/v0.1.3...v0.1.4) (2026-09-24)


### Bug Fixes

* **cli:** preserve OpenCode token totals ([6754e32](https://github.com/duyet/summa/commit/6754e32a5357ebf80bd02af4d6990e1dd245a035))
* **cli:** surface companion fetch failures ([994ae4f](https://github.com/duyet/summa/commit/994ae4f04963f20b20fe7f834db58968f13e59bc))
* **cli:** surface companion fetch failures ([bfc9bc2](https://github.com/duyet/summa/commit/bfc9bc26344e5bc29a74153b62fe0fee227b7fcf))
* **storage:** target configured ClickHouse database ([a15fbea](https://github.com/duyet/summa/commit/a15fbeac508e3bb83c2ef94846531e207e24a1c7))
* **telemetry:** make OpenCode ingestion observable ([c8589fb](https://github.com/duyet/summa/commit/c8589fb046ad8898bea5a43aa4b31ccb0cf72cc6))

## [0.1.3](https://github.com/duyet/summa/compare/v0.1.2...v0.1.3) (2026-09-17)


### Features

* beta/stable release channels, auto-update, Clerk login on landing page ([3a9b4dc](https://github.com/duyet/summa/commit/3a9b4dcbdbe6d75311bf64927860160453ed278a))
* **cli:** beta/stable release channels with auto-update ([ea16540](https://github.com/duyet/summa/commit/ea16540bd666d18c9c38488da2683d04d825dac4))
* **update:** show versions in update output ([e57aae5](https://github.com/duyet/summa/commit/e57aae5de863b338b7a42681a25f617d60193365))


### Bug Fixes

* **ci:** isolate env-mutating tests so cargo test can run in parallel ([28dc439](https://github.com/duyet/summa/commit/28dc4399740031f74cbe41ba503e87b0edead7dd)), closes [#103](https://github.com/duyet/summa/issues/103)
* **clickhouse:** implement atomic write with robust import_id exclusion (issue [#101](https://github.com/duyet/summa/issues/101)) ([b46ce07](https://github.com/duyet/summa/commit/b46ce0729d41cbd24055d5de848214e2ab6e91d0))
* **clickhouse:** implement atomic write with robust import_id exclusion (issue [#101](https://github.com/duyet/summa/issues/101)) ([6217b68](https://github.com/duyet/summa/commit/6217b68777b8144b0a5b9b701da40706caa3fbc3))
* **cli:** unbreak scopes_of move after HashMap insert ([#118](https://github.com/duyet/summa/issues/118)) ([4e933fe](https://github.com/duyet/summa/commit/4e933fe0826c7e72f2b72255cfac310026051af9))
* **deps:** update rust crate axum to 0.8 ([#87](https://github.com/duyet/summa/issues/87)) ([94a97dc](https://github.com/duyet/summa/commit/94a97dc19b26441bf7aade575b39646e99b17cdc))
* **deps:** update rust crate dirs to v7 ([#116](https://github.com/duyet/summa/issues/116)) ([3544a4c](https://github.com/duyet/summa/commit/3544a4ca3ddb5d453cead0a4fc507b5869c7dd5e))
* **install:** reject sidecars with extra checksum records ([#110](https://github.com/duyet/summa/issues/110)) ([d43a984](https://github.com/duyet/summa/commit/d43a98456d7261d166cb413eff1eedb740794742))
* **install:** verify sha256 checksums before extract ([#108](https://github.com/duyet/summa/issues/108)) ([089971c](https://github.com/duyet/summa/commit/089971cbfa2bca3ce12108e38d85e28e6110f3c2))
* **security:** gitignore env backups and live credential files ([#107](https://github.com/duyet/summa/issues/107)) ([0b8f432](https://github.com/duyet/summa/commit/0b8f43294482e7d3e39498f55920682cde3596df))
* **sink:** swap-table write so a crash cannot drop live rows ([#112](https://github.com/duyet/summa/issues/112)) ([2e7a7dd](https://github.com/duyet/summa/commit/2e7a7dd38dd21a74021995d9bc31acbbb72cf439)), closes [#101](https://github.com/duyet/summa/issues/101)
* **update:** match release asset names with .tar.gz suffix; backfill stable assets ([8e5b355](https://github.com/duyet/summa/commit/8e5b3553f43cf2f66feee5e4fccd373ff89aebf5))

## [0.1.2](https://github.com/duyet/summa/compare/v0.1.1...v0.1.2) (2026-08-21)


### Features

* **install:** make curl | bash install a real binary ([6047334](https://github.com/duyet/summa/commit/60473343d65e696f340f5134abadd12cdb16a29c))


### Bug Fixes

* **api:** ingest tenant isolation and payload caps ([dde447d](https://github.com/duyet/summa/commit/dde447d8f2c3327a8fb3a77a6c76867ee055cd34))
* **api:** stamp ingest tenant and cap payloads ([1369603](https://github.com/duyet/summa/commit/13696036c023742e9026d9766705f742abd61392))
* **deps:** update rust crate tower to 0.5 ([62e3336](https://github.com/duyet/summa/commit/62e33363d15b2fce11ecb913f1d5750ebc0d5a98))
* **deps:** update rust crate tower to 0.5 ([4f3fa08](https://github.com/duyet/summa/commit/4f3fa08672095d35dca02ad1088a0ff28b816e46))


### Documentation

* k3s Hermes as import client to summa.duyet.net ([992239b](https://github.com/duyet/summa/commit/992239bc0b6c97b394289100ff3e3b04824b4901))
