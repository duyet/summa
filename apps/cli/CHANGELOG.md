# Changelog

## [0.1.3](https://github.com/duyet/summa/compare/v0.1.2...v0.1.3) (2026-09-04)


### Features

* beta/stable release channels, auto-update, Clerk login on landing page ([3a9b4dc](https://github.com/duyet/summa/commit/3a9b4dcbdbe6d75311bf64927860160453ed278a))
* **cli:** beta/stable release channels with auto-update ([ea16540](https://github.com/duyet/summa/commit/ea16540bd666d18c9c38488da2683d04d825dac4))
* **update:** show versions in update output ([e57aae5](https://github.com/duyet/summa/commit/e57aae5de863b338b7a42681a25f617d60193365))


### Bug Fixes

* **clickhouse:** implement atomic write with robust import_id exclusion (issue [#101](https://github.com/duyet/summa/issues/101)) ([b46ce07](https://github.com/duyet/summa/commit/b46ce0729d41cbd24055d5de848214e2ab6e91d0))
* **clickhouse:** implement atomic write with robust import_id exclusion (issue [#101](https://github.com/duyet/summa/issues/101)) ([6217b68](https://github.com/duyet/summa/commit/6217b68777b8144b0a5b9b701da40706caa3fbc3))
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
