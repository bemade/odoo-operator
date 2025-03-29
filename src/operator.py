import kopf
import logging
import os
from kubernetes import client
from handlers.odoo_handler import OdooHandler
from webhook_server import ServiceModeWebhookServer

# Configure logging
logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

# Configure webhook server
webhook_host = os.getenv("WEBHOOK_HOST", "0.0.0.0")
webhook_port = 443
webhook_cert_path = "/etc/webhook/tls.crt"
webhook_key_path = "/etc/webhook/tls.key"
webhook_ca_path = "/etc/webhook/ca.crt"

# Service configuration for webhook
service_namespace = os.getenv("SERVICE_NAMESPACE", "default")
service_name = os.getenv("SERVICE_NAME", "odoo-operator-webhook")


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
            # Configure the webhook server with our custom ServiceModeWebhookServer
            settings.admission.server = ServiceModeWebhookServer(
                port=webhook_port,
                certfile=webhook_cert_path,
                pkeyfile=webhook_key_path,
                cafile=webhook_ca_path,
                service_name=service_name,
                service_namespace=service_namespace,
            )

            # Set managed to a name for the webhook configuration
            # This tells Kopf to create and manage the webhook configuration
            settings.admission.managed = "auto.kopf.dev"

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
def validate(body, old, new, **kwargs):
    """
    Validate the OdooInstance resource before it is created or updated.
    This is called by Kopf's validation webhook.
    """
    # Only validate if there's an upgrade request with a database
    _logger = logging.getLogger(__name__)
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


@kopf.timer("bemade.org", "v1", "odooinstances", interval=30.0)
def check_odoo_instance_periodic(body, **kwargs):
    """Periodic check for OdooInstances to handle any time-based operations.
    This delegates to the OdooHandler to perform all periodic checks.
    """
    handler = OdooHandler(body, **kwargs)
    handler.check_periodic()
