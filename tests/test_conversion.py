"""Tests for CRD conversion logic."""
import pytest
import sys
import os

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "../src"))

from conversion import convert_odoo_instance


def _make_obj(api_version: str, name: str = "test-instance") -> dict:
    return {
        "apiVersion": api_version,
        "kind": "OdooInstance",
        "metadata": {"name": name, "namespace": "default"},
        "spec": {"image": "odoo:18.0", "replicas": 1},
    }


class TestConvertOdooInstance:
    def test_v1alpha1_to_v1alpha2(self):
        obj = _make_obj("bemade.org/v1alpha1")
        result = convert_odoo_instance(obj, "bemade.org/v1alpha2")
        assert result["apiVersion"] == "bemade.org/v1alpha2"

    def test_v1alpha2_to_v1alpha1(self):
        obj = _make_obj("bemade.org/v1alpha2")
        result = convert_odoo_instance(obj, "bemade.org/v1alpha1")
        assert result["apiVersion"] == "bemade.org/v1alpha1"

    def test_same_version_passthrough(self):
        obj = _make_obj("bemade.org/v1alpha2")
        result = convert_odoo_instance(obj, "bemade.org/v1alpha2")
        assert result["apiVersion"] == "bemade.org/v1alpha2"

    def test_spec_preserved_on_upgrade(self):
        obj = _make_obj("bemade.org/v1alpha1")
        obj["spec"]["adminPassword"] = "secret"
        result = convert_odoo_instance(obj, "bemade.org/v1alpha2")
        assert result["spec"]["adminPassword"] == "secret"
        assert result["spec"]["image"] == "odoo:18.0"

    def test_spec_preserved_on_downgrade(self):
        obj = _make_obj("bemade.org/v1alpha2")
        obj["spec"]["replicas"] = 3
        result = convert_odoo_instance(obj, "bemade.org/v1alpha1")
        assert result["spec"]["replicas"] == 3

    def test_metadata_preserved(self):
        obj = _make_obj("bemade.org/v1alpha1", name="my-odoo")
        result = convert_odoo_instance(obj, "bemade.org/v1alpha2")
        assert result["metadata"]["name"] == "my-odoo"
        assert result["metadata"]["namespace"] == "default"

    def test_kind_preserved(self):
        obj = _make_obj("bemade.org/v1alpha1")
        result = convert_odoo_instance(obj, "bemade.org/v1alpha2")
        assert result["kind"] == "OdooInstance"
