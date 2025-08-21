import kopf
import logging
import os
from kubernetes import client
from kubernetes.client.rest import ApiException
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
def configure_webhook(settings: kopf.OperatorSettings, *args, **kwargs):
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
            )  # type: ignore

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


def _classify_and_raise_api_exception(e: ApiException):
    """Map K8s ApiException to kopf Permanent/Temporary errors.

    - Permanent: 400, 403, 422 and other 4xx by default (invalid spec, forbidden ops).
    - Temporary: 409, 429, 5xx (conflicts, rate limits, server issues).
    - PVC shrink specific message mapped to permanent stop.
    """
    body = getattr(e, "body", "") or ""
    status = getattr(e, "status", None)
    reason = getattr(e, "reason", "")
    msg = f"{status} {reason} {body}".strip()

    if status in (400, 403, 422):
        if "can not be less than previous value" in body:
            raise kopf.PermanentError("PVC shrink not allowed; stopping retries.")
        raise kopf.PermanentError(f"Permanent API error: {msg}")

    if status in (409, 429) or (status is not None and 500 <= status < 600):
        raise kopf.TemporaryError(f"Temporary API error: {msg}", delay=30)

    if status is not None and 400 <= status < 500:
        raise kopf.PermanentError(f"Permanent API error: {msg}")

    # Unknown, re-raise original to let Kopf decide/defaults apply
    raise e


@kopf.on.create("bemade.org", "v1", "odooinstances")
def create_fn(body, *args, **kwargs):
    handler = OdooHandler(body, **kwargs)
    try:
        handler.on_create()
    except ApiException as e:
        _classify_and_raise_api_exception(e)
    except (kopf.PermanentError, kopf.TemporaryError):
        # Let Kopf handle these as-is
        raise
    except Exception as e:
        # Default to temporary for unexpected errors
        raise kopf.TemporaryError(str(e), delay=30)


@kopf.on.update("bemade.org", "v1", "odooinstances")
def update_fn(body, *args, **kwargs):
    handler = OdooHandler(body, *args, **kwargs)
    try:
        handler.on_update()
    except ApiException as e:
        _classify_and_raise_api_exception(e)
    except (kopf.PermanentError, kopf.TemporaryError):
        raise
    except Exception as e:
        raise kopf.TemporaryError(str(e), delay=30)


@kopf.on.delete("bemade.org", "v1", "odooinstances")
def delete_fn(body, *args, **kwargs):
    handler = OdooHandler(body, *args, **kwargs)
    try:
        handler.on_delete()
    except ApiException as e:
        _classify_and_raise_api_exception(e)
    except (kopf.PermanentError, kopf.TemporaryError):
        raise
    except Exception as e:
        raise kopf.TemporaryError(str(e), delay=30)


@kopf.on.validate("bemade.org", "v1", "odooinstances")
def validate(body, old, new, *args, **kwargs):
    """
    Validate the OdooInstance resource before it is created or updated.
    This is called by Kopf's validation webhook.
    """
    # Only validate if there's an upgrade request with a database
    _logger = logging.getLogger(__name__)
    _logger.debug(f"In the validatioin webhook, body: {body}")
    _logger.debug(f"Old spec: {old}")
    _logger.debug(f"New spec: {new}")
    if not new:
        return

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


# GitSync CRD handlers removed - GitSync CRD is deprecated
# Sync jobs are now owned directly by OdooInstance CR and handled by GitSyncJobHandler


@kopf.timer("bemade.org", "v1", "odooinstances", interval=30.0)
def check_odoo_instance_periodic(body, *args, **kwargs):
    """Periodic check for OdooInstances to handle any time-based operations.
    This delegates to the OdooHandler to perform all periodic checks.
    """
    handler = OdooHandler(body, *args, **kwargs)
    handler.check_periodic()


def _is_odoo_job(body, *args, **kwargs):
    """Check if the job is a job owned by an OdooInstance."""
    logger.debug(f"Checking if job is an OdooInstance job")

    # Check job name pattern
    meta = body.get("metadata")
    if not meta:
        return False
    name = meta.get("name")
    owner_refs = meta.get("ownerReferences")
    if not name or not owner_refs:
        return False

    if "-restore-" not in name and "-backup-" not in name:
        return False

    # Check owner references to confirm it's owned by an OdooInstance
    for owner in owner_refs:
        if (
            owner.get("kind") == "OdooInstance"
            and owner.get("apiVersion") == "bemade.org/v1"
        ):
            return True
    return False


@kopf.on.field("batch", "v1", "jobs", when=_is_odoo_job, field="status.failed")
@kopf.on.field("batch", "v1", "jobs", when=_is_odoo_job, field="status.succeeded")
def on_job_completion(body, *args, **kwargs):
    """Handle completion (success or failure) of git sync job owned by OdooInstance."""
    logger.debug(f"Handling job completion.")
    owner_refs = [
        ref
        for ref in body.get("metadata", {}).get("ownerReferences", [])
        if ref.get("kind") == "OdooInstance"
    ]
    for owner in owner_refs:
        try:
            # Get the OdooInstance custom resource
            instance = client.CustomObjectsApi().get_namespaced_custom_object(
                "bemade.org",
                "v1",
                body.get("metadata", {}).get("namespace"),
                "odooinstances",
                owner.get("name"),
            )

            # Initialize OdooHandler
            odoo_handler = OdooHandler(body=instance, *args, **kwargs)
            odoo_handler.handle_job_completion(body)
        except Exception as e:
            logger.error(f"Error processing sync job: {e}")
