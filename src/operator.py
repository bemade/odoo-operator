import kopf
import logging
import os
from kubernetes import client
from handlers.odoo_handler import OdooHandler

# Configure logging
logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

# Configure webhook server
webhook_host = "0.0.0.0"
webhook_port = 443
webhook_cert_path = "/etc/webhook/tls.crt"
webhook_key_path = "/etc/webhook/tls.key"


@kopf.on.startup()
def configure_webhook(settings: kopf.OperatorSettings, **_):
    """
    Configure the webhook server on operator startup.
    This checks for certificate files and configures the webhook server if they exist.
    """
    logger.info("Configuring webhook server")

    # Check if webhook certificates exist
    if os.path.exists(webhook_cert_path) and os.path.exists(webhook_key_path):
        logger.info(
            f"Found webhook certificates at {webhook_cert_path} and {webhook_key_path}"
        )
        # Configure webhook server settings
        settings.admission.server = kopf.WebhookServer(
            host=webhook_host,
            port=webhook_port,
            certfile=webhook_cert_path,
            keyfile=webhook_key_path,
        )
    else:
        logger.warning(
            "Webhook certificates not found. Validation webhooks will not work."
        )


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


@kopf.on.validate("bemade.org", "v1", "odooinstances")
def validate_fn(body, old, new, **kwargs):
    """
    Validate the OdooInstance resource before it is created or updated.
    This is called by Kopf's validation webhook.
    """
    # Only validate if there's an upgrade request with a database
    new_spec = new.get("spec", {})
    upgrade_spec = new_spec.get("upgrade", {})
    database = upgrade_spec.get("database", "")
    modules = upgrade_spec.get("modules", [])

    # Skip validation if there's no upgrade request
    if not (
        upgrade_spec and database and isinstance(modules, list) and len(modules) > 0
    ):
        return

    # Create a handler to validate the database
    handler = OdooHandler(new, **kwargs)

    # Validate that the database exists and belongs to the Odoo user
    exists, error_message = handler.validate_database_exists(database)

    if not exists:
        raise kopf.AdmissionError(error_message)


@kopf.timer("batch", "v1", "jobs", interval=30.0, labels={"type": "upgrade-job"})
def check_upgrade_job_completion(name, namespace, labels, **kwargs):
    """Check if an upgrade job has completed and delegate to OdooHandler for post-completion tasks."""
    # Only process jobs with the upgrade-job label and app-instance label
    if (
        "type" not in labels
        or labels["type"] != "upgrade-job"
        or "app-instance" not in labels
    ):
        return

    # Create an OdooHandler instance from the job info
    handler = OdooHandler.from_job_info(namespace, labels["app-instance"])

    # If we got a valid handler, delegate the job check to it
    if handler:
        handler.handle_upgrade_job_check()


@kopf.timer("bemade.org", "v1", "odooinstances", interval=60.0)
def check_scheduled_upgrades(body, **kwargs):
    """Check if any OdooInstances have scheduled upgrades that need to be executed."""
    handler = OdooHandler(body, **kwargs)
    handler.check_scheduled_upgrade()
