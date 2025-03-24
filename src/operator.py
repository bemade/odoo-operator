import kopf
import logging
from kubernetes import client
from handlers.odoo_handler import OdooHandler


@kopf.on.create("bemade.org", "v1", "odooinstances")
def create_fn(body, **kwargs):
    handler = OdooHandler(body, **kwargs)
    handler.on_create()


@kopf.on.update("bemade.org", "v1", "odooinstances")
def update_fn(body, **kwargs):
    handler = OdooHandler(body, **kwargs)
    handler.on_update()


@kopf.on.delete("bemade.org", "v1", "odooinstances")
def delete_fn(body, **kwargs):
    handler = OdooHandler(body, **kwargs)
    handler.on_delete()


@kopf.timer("batch", "v1", "jobs", interval=30.0, labels={"type": "upgrade-job"})
def check_upgrade_job_completion(name, namespace, labels, **kwargs):
    """Check if an upgrade job has completed and delegate to OdooHandler for post-completion tasks."""
    # Only process jobs with the upgrade-job label and app-instance label
    if "type" not in labels or labels["type"] != "upgrade-job" or "app-instance" not in labels:
        return

    # Create an OdooHandler instance from the job info
    handler = OdooHandler.from_job_info(namespace, labels["app-instance"])

    # If we got a valid handler, delegate the job check to it
    if handler:
        handler.handle_upgrade_job_check()
