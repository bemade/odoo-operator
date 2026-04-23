# Changelog

## [1.10.0](https://github.com/bemade/odoo-operator/compare/v1.9.0...v1.10.0) (2026-04-23)


### Features

* **instance:** productionInstanceRef auto-clones staging from prod ([760ef55](https://github.com/bemade/odoo-operator/commit/760ef554e0b3dfa0f7ec24e23e773277e60ca0d6))
* **instance:** productionInstanceRef auto-clones staging from prod ([9b6687b](https://github.com/bemade/odoo-operator/commit/9b6687b0b414f02973b44d23a9968a03e08351c2))

## [1.9.0](https://github.com/bemade/odoo-operator/compare/v1.8.0...v1.9.0) (2026-04-22)


### Features

* **mail:** staging instances auto-redirect SMTP to Mailpit (BREAKING) ([aedf19c](https://github.com/bemade/odoo-operator/commit/aedf19c1abfc132263e8be634b8d0a58f8ae15d8))
* **mail:** staging instances auto-redirect SMTP to Mailpit (BREAKING) ([9186467](https://github.com/bemade/odoo-operator/commit/9186467e068c7ead1ba32ede06cce7df32e8236b))

## [1.8.0](https://github.com/bemade/odoo-operator/compare/v1.7.0...v1.8.0) (2026-04-22)


### Features

* **instance:** environment label (Staging default) for Calico policies ([1b89d4f](https://github.com/bemade/odoo-operator/commit/1b89d4f9b583db90388a29e19b76fe3b6ef3b7b0))
* **instance:** environment label (Staging default) for Calico policies ([5bbc226](https://github.com/bemade/odoo-operator/commit/5bbc226d0d7d885621a0c061a1ecb79a76c42144))
* **staging:** Phase 1 — OdooStagingRefreshJob + cloning pipeline ([dd0eb9d](https://github.com/bemade/odoo-operator/commit/dd0eb9da82134bf12065699683052ca1e3a8132f))
* **staging:** Phase 1 — OdooStagingRefreshJob CRD + cloning pipeline ([9e902df](https://github.com/bemade/odoo-operator/commit/9e902df51d9abd81be224f96c0ca772e07e5f2b6))


### Bug Fixes

* **restore:** harden pipeline against un-neutralized DB incidents ([d112c7e](https://github.com/bemade/odoo-operator/commit/d112c7e5b4388fcfb7285a5299371683f8607df7))
* **restore:** harden pipeline against un-neutralized DB incidents ([d112c7e](https://github.com/bemade/odoo-operator/commit/d112c7e5b4388fcfb7285a5299371683f8607df7))
* **restore:** harden pipeline against un-neutralized DB incidents ([c3b04d8](https://github.com/bemade/odoo-operator/commit/c3b04d804a6662a7e820830b30fb8b90715d1d36))
* **staging:** live-test fixes for the clone pipeline ([9cffed3](https://github.com/bemade/odoo-operator/commit/9cffed33e04d655a497246f9e6af40b6f57cfda3))
* **starting:** detect and recover from stuck volume mounts ([8a60680](https://github.com/bemade/odoo-operator/commit/8a60680826e6fac16dd525db7770257b44de3670))
* **starting:** detect and recover from stuck volume mounts ([aca4b7e](https://github.com/bemade/odoo-operator/commit/aca4b7e50803b77a1a0e5e869b63138de535fe3e))
* **state-machine:** avoid phase flap with queued backup jobs ([c912eaf](https://github.com/bemade/odoo-operator/commit/c912eafc23b82a6caec0ac80915122c4924276d1))
* **state-machine:** avoid phase flap with queued backup jobs ([c912eaf](https://github.com/bemade/odoo-operator/commit/c912eafc23b82a6caec0ac80915122c4924276d1))
* **state-machine:** avoid phase flap with queued backup jobs ([38031db](https://github.com/bemade/odoo-operator/commit/38031db2db1f2af17ad5767481db77f18ab22bfd))

## [1.7.0](https://github.com/bemade/odoo-operator/compare/v1.6.5...v1.7.0) (2026-04-15)


### Features

* add database cluster migration (MigratingDatabase / FinalizingDatabaseMigration) ([3fc5a59](https://github.com/bemade/odoo-operator/commit/3fc5a59b28b3e858f687c1b63828580ef8abd5a0))
* database cluster migration ([de9d00b](https://github.com/bemade/odoo-operator/commit/de9d00be4786d1f3e05c3b1c35f27393aef8c267))


### Bug Fixes

* regenerate CRDs during release chart job ([74e5b54](https://github.com/bemade/odoo-operator/commit/74e5b5432c3385c35fb2ba0a578b637311d05f97)), closes [#55](https://github.com/bemade/odoo-operator/issues/55)
* use proper issuer for self-signed cert and CA injection ([9283d90](https://github.com/bemade/odoo-operator/commit/9283d9047dc6653ed25c0ddd191c4b039995890a))
* use proper issuer for self-signed cert and CA injection ([c9844f8](https://github.com/bemade/odoo-operator/commit/c9844f8177efeb39dca141c1f631347a1dabf017))

## [1.6.5](https://github.com/bemade/odoo-operator/compare/v1.6.4...v1.6.5) (2026-04-13)


### Bug Fixes

* split migration finalization into separate phase, fix PVC rebind race ([#69](https://github.com/bemade/odoo-operator/issues/69)) ([7ada598](https://github.com/bemade/odoo-operator/commit/7ada59883209a0d7262d6c7bbf3baedf9b69a2b7))

## [1.6.4](https://github.com/bemade/odoo-operator/compare/v1.6.3...v1.6.4) (2026-04-12)


### Bug Fixes

* split migration finalization into separate phase, fix PVC rebind race ([#67](https://github.com/bemade/odoo-operator/issues/67)) ([ddb2cc5](https://github.com/bemade/odoo-operator/commit/ddb2cc55eabcfad58ac815ceaa5e23ef8216a4bf))

## [1.6.3](https://github.com/bemade/odoo-operator/compare/v1.6.2...v1.6.3) (2026-04-12)


### Bug Fixes

* make CompleteFilestoreMigration idempotent across retries ([#65](https://github.com/bemade/odoo-operator/issues/65)) ([2de6a0e](https://github.com/bemade/odoo-operator/commit/2de6a0e254ac5448f49959489ec2fa77b9f84953))

## [1.6.2](https://github.com/bemade/odoo-operator/compare/v1.6.1...v1.6.2) (2026-04-12)


### Bug Fixes

* add PVC delete and PV patch permissions to operator RBAC ([#62](https://github.com/bemade/odoo-operator/issues/62)) ([c4a2708](https://github.com/bemade/odoo-operator/commit/c4a270829ee2e7285a0731d775b046a826695cd4))

## [1.6.1](https://github.com/bemade/odoo-operator/compare/v1.6.0...v1.6.1) (2026-04-12)


### Bug Fixes

* handle FUSE filesystem restrictions in migration rsync ([#60](https://github.com/bemade/odoo-operator/issues/60)) ([779786f](https://github.com/bemade/odoo-operator/commit/779786f477e61e38aa9b2e63d561fe71148fb2b1))

## [1.6.0](https://github.com/bemade/odoo-operator/compare/v1.5.1...v1.6.0) (2026-04-12)


### Features

* add demo data flag to InitSpec and OdooInitJob CRDs ([#57](https://github.com/bemade/odoo-operator/issues/57)) ([478e032](https://github.com/bemade/odoo-operator/commit/478e032a5dc144a44e4d3e8f1c2ffe8de867e7c7))


### Bug Fixes

* exclude JuiceFS virtual files from migration rsync ([#59](https://github.com/bemade/odoo-operator/issues/59)) ([0b67e4d](https://github.com/bemade/odoo-operator/commit/0b67e4d15e5230ec783ca71d06e9f06bd88f3e58))

## [1.5.1](https://github.com/bemade/odoo-operator/compare/v1.5.0...v1.5.1) (2026-04-12)


### Bug Fixes

* regenerate CRDs with MigratingFilestore phase ([#54](https://github.com/bemade/odoo-operator/issues/54)) ([cf9167d](https://github.com/bemade/odoo-operator/commit/cf9167d9db711e9bb0cd1b85f7768c3d3327df36))

## [1.5.0](https://github.com/bemade/odoo-operator/compare/v1.4.2...v1.5.0) (2026-04-12)


### Features

* automatic filestore StorageClass migration ([#52](https://github.com/bemade/odoo-operator/issues/52)) ([e0f9f01](https://github.com/bemade/odoo-operator/commit/e0f9f013d4cf76ea46f6b971b857c8c34e792a3a))

## [1.4.2](https://github.com/bemade/odoo-operator/compare/v1.4.1...v1.4.2) (2026-04-08)


### Bug Fixes

* pass --with-demo flag when init.demo is true ([#50](https://github.com/bemade/odoo-operator/issues/50)) ([edbac81](https://github.com/bemade/odoo-operator/commit/edbac81a5f489a098d1995fec2e9be0632e7a4b6))

## [1.4.1](https://github.com/bemade/odoo-operator/compare/v1.4.0...v1.4.1) (2026-04-08)


### Bug Fixes

* override PGDATABASE env var on deployments ([#48](https://github.com/bemade/odoo-operator/issues/48)) ([2b22651](https://github.com/bemade/odoo-operator/commit/2b226511c3f56fc2b1836d7f8d9adf82ab19bd98))

## [1.4.0](https://github.com/bemade/odoo-operator/compare/v1.3.1...v1.4.0) (2026-03-31)


### Features

* add demo data flag to InitSpec and OdooInitJob CRDs ([#46](https://github.com/bemade/odoo-operator/issues/46)) ([d92eba3](https://github.com/bemade/odoo-operator/commit/d92eba34040a2142a612504bbe4047cddac0b82a))

## [1.3.1](https://github.com/bemade/odoo-operator/compare/v1.3.0...v1.3.1) (2026-03-27)


### Bug Fixes

* drop database via SQL before restore to handle invalid DBs ([#43](https://github.com/bemade/odoo-operator/issues/43)) ([5794c80](https://github.com/bemade/odoo-operator/commit/5794c8059ca019f4a3400092795e5b1e9b663648))

## [1.3.0](https://github.com/bemade/odoo-operator/compare/v1.2.0...v1.3.0) (2026-03-23)


### Features

* respect scheduledTime on OdooUpgradeJob before transitioning ([#41](https://github.com/bemade/odoo-operator/issues/41)) ([20b67cb](https://github.com/bemade/odoo-operator/commit/20b67cbd20cd9e421c88ab28164329f99df0584a))

## [1.2.0](https://github.com/bemade/odoo-operator/compare/v1.1.0...v1.2.0) (2026-03-17)


### Features

* ensure report.url system parameter points to in-cluster web service ([#40](https://github.com/bemade/odoo-operator/issues/40)) ([4e9f176](https://github.com/bemade/odoo-operator/commit/4e9f176082ac10af7b88fde706a3f6051f39cbee))


### Bug Fixes

* add grace period to cron liveness probe to prevent crash loop ([#38](https://github.com/bemade/odoo-operator/issues/38)) ([17489a4](https://github.com/bemade/odoo-operator/commit/17489a4f3c5f2cc07adfc5681498eab6e658ea88))

## [1.1.0](https://github.com/bemade/odoo-operator/compare/v1.0.0...v1.1.0) (2026-03-04)


### Features

* add Gateway API support via opt-in gatewayRef field ([#36](https://github.com/bemade/odoo-operator/issues/36)) ([c7df401](https://github.com/bemade/odoo-operator/commit/c7df401e1df1547bc1932d7b848d06b66cb26821))

## [1.0.0](https://github.com/bemade/odoo-operator/compare/v0.14.0...v1.0.0) (2026-03-02)


### ⚠ BREAKING CHANGES

* OdooInstances now auto-initialize by default. Set

### Features

* auto-initialize OdooInstance database by default ([#34](https://github.com/bemade/odoo-operator/issues/34)) ([3bdb9b9](https://github.com/bemade/odoo-operator/commit/3bdb9b9aedd81d0f8eb1636821d6b7f821f40951))

## [0.14.0](https://github.com/bemade/odoo-operator/compare/v0.13.12...v0.14.0) (2026-03-02)


### Features

* add optional database name to OdooInstance spec ([#32](https://github.com/bemade/odoo-operator/issues/32)) ([d57cb73](https://github.com/bemade/odoo-operator/commit/d57cb7345df0f65fd4cf7b7c700c8678f08e6f6f))

## [0.13.12](https://github.com/bemade/odoo-operator/compare/v0.13.11...v0.13.12) (2026-03-02)


### Bug Fixes

* move max_cron_threads from hardcoded CLI arg to odoo.conf ([#29](https://github.com/bemade/odoo-operator/issues/29)) ([eb2aeb7](https://github.com/bemade/odoo-operator/commit/eb2aeb7574a1f02c7650c72127f0b36ed4363fb1))

## [0.13.11](https://github.com/bemade/odoo-operator/compare/v0.13.10...v0.13.11) (2026-03-02)


### Bug Fixes

* cron liveness probe v2 ([#27](https://github.com/bemade/odoo-operator/issues/27)) ([297d693](https://github.com/bemade/odoo-operator/commit/297d693fcd81cd6ada41e18a25f03af0f32b970e))

## [0.13.10](https://github.com/bemade/odoo-operator/compare/v0.13.9...v0.13.10) (2026-03-01)


### Bug Fixes

* add v1alpha2 legacy CRD version for Python operator upgrades ([#22](https://github.com/bemade/odoo-operator/issues/22)) ([e3c70d8](https://github.com/bemade/odoo-operator/commit/e3c70d890a9bf534ee2f2087a747a375b9c4458a))

## [0.13.9](https://github.com/bemade/odoo-operator/compare/v0.13.8...v0.13.9) (2026-02-27)


### Bug Fixes

* publish helm chart to GHCR OCI registry ([#19](https://github.com/bemade/odoo-operator/issues/19)) ([32404b0](https://github.com/bemade/odoo-operator/commit/32404b0d6e36faf9973173f44792f3aed54506a9))

## [0.13.8](https://github.com/bemade/odoo-operator/compare/v0.13.7...v0.13.8) (2026-02-26)


### Bug Fixes

* add liveness probe to cron pods to detect dead cron threads ([#17](https://github.com/bemade/odoo-operator/issues/17)) ([ca15d5f](https://github.com/bemade/odoo-operator/commit/ca15d5f65db104404a6e93e1d8b812a495fa6ffa))

## [0.13.7](https://github.com/bemade/odoo-operator/compare/v0.13.6...v0.13.7) (2026-02-26)


### Bug Fixes

* scale down cron deployment during restore to avoid pooler stale connections ([#15](https://github.com/bemade/odoo-operator/issues/15)) ([5d7a522](https://github.com/bemade/odoo-operator/commit/5d7a5226e92c0bda0a10d12e8ff475af9e73661a))

## [0.13.6](https://github.com/bemade/odoo-operator/compare/v0.13.5...v0.13.6) (2026-02-26)


### Bug Fixes

* fold release pipeline into release-please workflow to avoid GITHUB_TOKEN cascade restriction ([#12](https://github.com/bemade/odoo-operator/issues/12)) ([e934f88](https://github.com/bemade/odoo-operator/commit/e934f88b13cde3abefdb132544177a98297d404a))

## [0.13.5](https://github.com/bemade/odoo-operator/compare/v0.13.4...v0.13.5) (2026-02-26)


### Bug Fixes

* trigger release workflow on GitHub release published event ([#10](https://github.com/bemade/odoo-operator/issues/10)) ([ed0c97f](https://github.com/bemade/odoo-operator/commit/ed0c97fc099128d0dbeb2bfce3e93eb7855833d7))

## [0.13.4](https://github.com/bemade/odoo-operator/compare/v0.13.3...v0.13.4) (2026-02-26)


### Bug Fixes

* remove pre-drop for backup.zip restore path to avoid pooler race ([d9c4d26](https://github.com/bemade/odoo-operator/commit/d9c4d262c77e73670288ff6854d9f62ed0006bfa))
* use unprefixed v* tags for release-please to match release workflow trigger ([#8](https://github.com/bemade/odoo-operator/issues/8)) ([893989f](https://github.com/bemade/odoo-operator/commit/893989ff6bf05d7d6824cf506ab33d7f55691ef4))

## [0.13.3](https://github.com/bemade/odoo-operator/compare/odoo-operator-0.13.2...odoo-operator-v0.13.3) (2026-02-26)


### Bug Fixes

* remove pre-drop for backup.zip restore path to avoid pooler race ([d9c4d26](https://github.com/bemade/odoo-operator/commit/d9c4d262c77e73670288ff6854d9f62ed0006bfa))
