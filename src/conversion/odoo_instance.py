import logging

logger = logging.getLogger(__name__)


def convert_odoo_instance(obj: dict, desired_api_version: str) -> dict:
    current = obj["apiVersion"]
    key = (current, desired_api_version)
    fn = _CONVERSIONS.get(key)
    if fn is None:
        obj["apiVersion"] = desired_api_version
        return obj
    return fn(obj)


def _v1alpha1_to_v1alpha2(obj: dict) -> dict:
    """v1alpha1 → v1alpha2: schemas are identical, just bump version."""
    logger.info(
        "Converting OdooInstance %s v1alpha1 → v1alpha2",
        obj["metadata"]["name"],
    )
    obj["apiVersion"] = "bemade.org/v1alpha2"
    return obj


def _v1alpha2_to_v1alpha1(obj: dict) -> dict:
    """v1alpha2 → v1alpha1: downgrade for backwards-compat reads."""
    logger.info(
        "Converting OdooInstance %s v1alpha2 → v1alpha1",
        obj["metadata"]["name"],
    )
    obj["apiVersion"] = "bemade.org/v1alpha1"
    return obj


_CONVERSIONS = {
    ("bemade.org/v1alpha1", "bemade.org/v1alpha2"): _v1alpha1_to_v1alpha2,
    ("bemade.org/v1alpha2", "bemade.org/v1alpha1"): _v1alpha2_to_v1alpha1,
}
