# Changelog

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
