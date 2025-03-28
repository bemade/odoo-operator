import kopf
import logging
import os
from kubernetes import client
from handlers.odoo_handler import OdooHandler

# Configure logging
logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

# Configure webhook server
webhook_host = os.getenv("WEBHOOK_HOST", "0.0.0.0")
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
        try:
            settings.admission.server = kopf.WebhookServer(
                host=webhook_host,
                port=webhook_port,
                certfile=webhook_cert_path,
                pkeyfile=webhook_key_path,
            )
            settings.admission.managed = 'auto.kopf.dev'

            logger.info("Webhook server configured successfully")

            # Log webhook server settings
            logger.info(f"Webhook server listening on {webhook_host}:{webhook_port}")
        except Exception as e:
            logger.error(f"Error configuring webhook server: {e}")
    else:
        logger.warning(
            "Webhook certificates not found. Validation webhooks will not work."
        )
        if not os.path.exists(webhook_cert_path):
            logger.error(f"Certificate file not found: {webhook_cert_path}")
        if not os.path.exists(webhook_key_path):
            logger.error(f"Key file not found: {webhook_key_path}")


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
    _logger.debug(f"In the validatioin webhook, body: {body}")
    _logger.debug(f"Old spec: {old}")
    _logger.debug(f"New spec: {new}")


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
def check_upgrade_job_completion(name, namespace, labels, status, **kwargs):
    """Check if an upgrade job has completed and delegate to OdooHandler for post-completion tasks."""
    # Only process jobs with the upgrade-job label and app-instance label
    if (
        "type" not in labels
        or labels["type"] != "upgrade-job"
        or "app-instance" not in labels
    ):
        return

    # Skip jobs that have already been processed
    # We'll add an annotation to the job when we process it
    annotations = kwargs.get("meta", {}).get("annotations", {})
    if annotations.get("odoo-operator/upgrade-handled") == "true":
        logger.info(f"Skipping already handled upgrade job: {name}")
        return

    # Get the app instance name from the labels
    app_instance = labels.get("app-instance")

    # Check if the job is complete
    job_complete = False

    # The status structure can vary, so we need to handle it carefully
    if status:
        # Check for completion in status.conditions if it exists
        if hasattr(status, "conditions") and status.conditions:
            for condition in status.conditions:
                if condition.type == "Complete" and condition.status == "True":
                    job_complete = True
                    break
        # Also check status.succeeded which is more commonly available
        elif hasattr(status, "succeeded") and status.succeeded:
            job_complete = True

    if not job_complete:
        logger.debug(f"Job {name} in namespace {namespace} is not complete yet")
        return

    # Create an OdooHandler from the job info
    handler = OdooHandler.from_job_info(namespace, app_instance)
    if not handler:
        logger.error(
            f"Could not create OdooHandler for job {name} in namespace {namespace}"
        )
        return

    # Handle the upgrade job completion
    success = handler.handle_upgrade_job_check(name)

    if success:
        # Mark the job as handled with an annotation
        patch = {"metadata": {"annotations": {"odoo-operator/upgrade-handled": "true"}}}
        api = client.BatchV1Api()
        api.patch_namespaced_job(name, namespace, patch)
        logger.info(f"Marked job {name} as handled")


@kopf.timer("bemade.org", "v1", "odooinstances", interval=60.0)
def check_scheduled_upgrades(body, **kwargs):
    """Check if any OdooInstances have scheduled upgrades that need to be executed."""
    handler = OdooHandler(body, **kwargs)
    handler.check_scheduled_upgrade()
