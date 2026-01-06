import kopf
import logging
import os
from kubernetes import client
from kubernetes.client.rest import ApiException
from handlers.odoo_handler import OdooHandler
from handlers.backup_job_handler import OdooBackupJobHandler
from handlers.restore_job_handler import OdooRestoreJobHandler
from handlers.upgrade_job_handler import OdooUpgradeJobHandler
from handlers.init_job_handler import OdooInitJobHandler
from typing import cast
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


@kopf.on.resume("bemade.org", "v1", "odooinstances")
def restart_fn(*args, **kwargs):
    update_fn(*args, **kwargs)


def _classify_and_raise_api_exception(e: ApiException):
    """Map K8s ApiException to kopf Permanent/Temporary errors.

    - Permanent: 400, 403, 422 and other 4xx by default (invalid spec, forbidden ops).
    - Temporary: 409, 429, 5xx (conflicts, rate limits, server issues).
    - PVC shrink specific message mapped to permanent stop.
    """
    body = cast(str, getattr(e, "body", "") or "")
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


# ============== OdooBackupJob handlers ==============


@kopf.on.create("bemade.org", "v1", "odoobackupjobs")
def create_backup_job(body, *args, **kwargs):
    """Handle creation of OdooBackupJob CR."""
    handler = OdooBackupJobHandler(body, **kwargs)
    try:
        handler.on_create()
    except ApiException as e:
        _classify_and_raise_api_exception(e)
    except (kopf.PermanentError, kopf.TemporaryError):
        raise
    except Exception as e:
        raise kopf.TemporaryError(str(e), delay=30)


@kopf.on.update("bemade.org", "v1", "odoobackupjobs")
def update_backup_job(body, *args, **kwargs):
    """Handle update of OdooBackupJob CR."""
    handler = OdooBackupJobHandler(body, **kwargs)
    try:
        handler.on_update()
    except ApiException as e:
        _classify_and_raise_api_exception(e)
    except (kopf.PermanentError, kopf.TemporaryError):
        raise
    except Exception as e:
        raise kopf.TemporaryError(str(e), delay=30)


# ============== OdooRestoreJob handlers ==============


@kopf.on.create("bemade.org", "v1", "odoorestorejobs")
def create_restore_job(body, *args, **kwargs):
    """Handle creation of OdooRestoreJob CR."""
    handler = OdooRestoreJobHandler(body, **kwargs)
    try:
        handler.on_create()
    except ApiException as e:
        _classify_and_raise_api_exception(e)
    except (kopf.PermanentError, kopf.TemporaryError):
        raise
    except Exception as e:
        raise kopf.TemporaryError(str(e), delay=30)


@kopf.on.update("bemade.org", "v1", "odoorestorejobs")
def update_restore_job(body, *args, **kwargs):
    """Handle update of OdooRestoreJob CR."""
    handler = OdooRestoreJobHandler(body, **kwargs)
    try:
        handler.on_update()
    except ApiException as e:
        _classify_and_raise_api_exception(e)
    except (kopf.PermanentError, kopf.TemporaryError):
        raise
    except Exception as e:
        raise kopf.TemporaryError(str(e), delay=30)


# Job owner kinds that we watch for completion
_JOB_OWNER_KINDS = {
    "OdooInstance",
    "OdooBackupJob",
    "OdooRestoreJob",
    "OdooUpgradeJob",
    "OdooInitJob",
}


def _is_operator_job(body, *args, **kwargs):
    """Check if the job is owned by one of our CRDs."""
    owner_refs = body.get("metadata", {}).get("ownerReferences", [])
    for owner in owner_refs:
        if (
            owner.get("apiVersion") == "bemade.org/v1"
            and owner.get("kind") in _JOB_OWNER_KINDS
        ):
            return True
    return False


# ============== OdooUpgradeJob handlers ==============


@kopf.on.create("bemade.org", "v1", "odooupgradejobs")
def create_upgrade_job(body, *args, **kwargs):
    """Handle creation of OdooUpgradeJob CR."""
    handler = OdooUpgradeJobHandler(body, **kwargs)
    try:
        handler.on_create()
    except ApiException as e:
        _classify_and_raise_api_exception(e)
    except (kopf.PermanentError, kopf.TemporaryError):
        raise
    except Exception as e:
        raise kopf.TemporaryError(str(e), delay=30)


@kopf.on.update("bemade.org", "v1", "odooupgradejobs")
def update_upgrade_job(body, *args, **kwargs):
    """Handle update of OdooUpgradeJob CR."""
    handler = OdooUpgradeJobHandler(body, **kwargs)
    try:
        handler.on_update()
    except ApiException as e:
        _classify_and_raise_api_exception(e)
    except (kopf.PermanentError, kopf.TemporaryError):
        raise
    except Exception as e:
        raise kopf.TemporaryError(str(e), delay=30)


# ============== OdooInitJob handlers ==============


@kopf.on.create("bemade.org", "v1", "odooinitjobs")
def create_init_job(body, *args, **kwargs):
    """Handle creation of OdooInitJob CR."""
    handler = OdooInitJobHandler(body, **kwargs)
    try:
        handler.on_create()
    except ApiException as e:
        _classify_and_raise_api_exception(e)
    except (kopf.PermanentError, kopf.TemporaryError):
        raise
    except Exception as e:
        raise kopf.TemporaryError(str(e), delay=30)


@kopf.on.update("bemade.org", "v1", "odooinitjobs")
def update_init_job(body, *args, **kwargs):
    """Handle update of OdooInitJob CR."""
    handler = OdooInitJobHandler(body, **kwargs)
    try:
        handler.on_update()
    except ApiException as e:
        _classify_and_raise_api_exception(e)
    except (kopf.PermanentError, kopf.TemporaryError):
        raise
    except Exception as e:
        raise kopf.TemporaryError(str(e), delay=30)


@kopf.on.field("batch", "v1", "jobs", when=_is_operator_job, field="status.failed")
@kopf.on.field("batch", "v1", "jobs", when=_is_operator_job, field="status.succeeded")
def on_job_completion(body, *args, **kwargs):
    """Handle completion (success or failure) of jobs owned by our CRDs."""
    namespace = body.get("metadata", {}).get("namespace")
    owner_refs = body.get("metadata", {}).get("ownerReferences", [])

    for owner in owner_refs:
        if owner.get("apiVersion") != "bemade.org/v1":
            continue

        kind = owner.get("kind")
        owner_name = owner.get("name")

        try:
            if kind == "OdooInstance":
                instance = client.CustomObjectsApi().get_namespaced_custom_object(
                    "bemade.org", "v1", namespace, "odooinstances", owner_name
                )
                odoo_handler = OdooHandler(body=instance, *args, **kwargs)
                odoo_handler.handle_job_completion(body)

            elif kind == "OdooBackupJob":
                backup_job = client.CustomObjectsApi().get_namespaced_custom_object(
                    "bemade.org", "v1", namespace, "odoobackupjobs", owner_name
                )
                handler = OdooBackupJobHandler(backup_job, **kwargs)
                handler.check_job_status()

            elif kind == "OdooRestoreJob":
                restore_job = client.CustomObjectsApi().get_namespaced_custom_object(
                    "bemade.org", "v1", namespace, "odoorestorejobs", owner_name
                )
                handler = OdooRestoreJobHandler(restore_job, **kwargs)
                handler.check_job_status()

            elif kind == "OdooUpgradeJob":
                upgrade_job = client.CustomObjectsApi().get_namespaced_custom_object(
                    "bemade.org", "v1", namespace, "odooupgradejobs", owner_name
                )
                handler = OdooUpgradeJobHandler(upgrade_job, **kwargs)
                handler.check_job_status()

            elif kind == "OdooInitJob":
                init_job = client.CustomObjectsApi().get_namespaced_custom_object(
                    "bemade.org", "v1", namespace, "odooinitjobs", owner_name
                )
                handler = OdooInitJobHandler(init_job, **kwargs)
                handler.check_job_status()

        except Exception as e:
            logger.error(
                f"Error processing job completion for {kind}/{owner_name}: {e}"
            )
