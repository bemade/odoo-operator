# Changelog

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
