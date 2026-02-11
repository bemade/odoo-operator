.PHONY: test lint coverage sync

sync:
	uv sync

test:
	uv run pytest -q

coverage:
	uv run pytest --cov=handlers --cov=operator --cov-report=term-missing

lint:
	helm lint charts/odoo-operator
