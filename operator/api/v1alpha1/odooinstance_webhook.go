/*
Copyright 2026 Bemade Inc..

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

package v1alpha1

import (
	"context"
	"fmt"

	"k8s.io/apimachinery/pkg/api/resource"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/webhook/admission"
)

// +kubebuilder:webhook:path=/validate-bemade-org-v1alpha1-odooinstance,mutating=false,failurePolicy=fail,sideEffects=None,groups=bemade.org,resources=odooinstances,verbs=update,versions=v1alpha1,name=vodooinstance.kb.io,admissionReviewVersions=v1

// OdooInstanceValidator validates OdooInstance resources.
type OdooInstanceValidator struct{}

// SetupWebhookWithManager registers the validating webhook with the manager.
func SetupOdooInstanceWebhookWithManager(mgr ctrl.Manager) error {
	return ctrl.NewWebhookManagedBy(mgr, &OdooInstance{}).
		WithValidator(&OdooInstanceValidator{}).
		Complete()
}

func (v *OdooInstanceValidator) ValidateCreate(_ context.Context, _ *OdooInstance) (admission.Warnings, error) {
	return nil, nil
}

func (v *OdooInstanceValidator) ValidateUpdate(_ context.Context, old, new *OdooInstance) (admission.Warnings, error) {
	// Reject storage size decreases — PVCs cannot shrink.
	if old.Spec.Filestore != nil && new.Spec.Filestore != nil &&
		old.Spec.Filestore.StorageSize != "" && new.Spec.Filestore.StorageSize != "" {
		oldQty := resource.MustParse(old.Spec.Filestore.StorageSize)
		newQty := resource.MustParse(new.Spec.Filestore.StorageSize)
		if newQty.Cmp(oldQty) < 0 {
			return nil, fmt.Errorf(
				"spec.filestore.storageSize: cannot decrease storage size from %s to %s",
				old.Spec.Filestore.StorageSize, new.Spec.Filestore.StorageSize,
			)
		}
	}

	// Reject database cluster changes — cluster migration is not yet implemented.
	oldCluster := ""
	if old.Spec.Database != nil {
		oldCluster = old.Spec.Database.Cluster
	}
	newCluster := ""
	if new.Spec.Database != nil {
		newCluster = new.Spec.Database.Cluster
	}
	if oldCluster != "" && newCluster != oldCluster {
		return nil, fmt.Errorf(
			"spec.database.cluster: changing the postgres cluster from %q to %q is not supported",
			oldCluster, newCluster,
		)
	}

	// Reject storageClass changes — the filestore PVC storageClass is immutable after creation.
	// To migrate storage, manipulate the PVC/PV directly; do not change the spec.
	oldClass := ""
	if old.Spec.Filestore != nil {
		oldClass = old.Spec.Filestore.StorageClass
	}
	newClass := ""
	if new.Spec.Filestore != nil {
		newClass = new.Spec.Filestore.StorageClass
	}
	if oldClass != "" && newClass != oldClass {
		return nil, fmt.Errorf(
			"spec.filestore.storageClass: cannot change storage class from %q to %q; the filestore PVC is immutable after creation",
			oldClass, newClass,
		)
	}

	return nil, nil
}

func (v *OdooInstanceValidator) ValidateDelete(_ context.Context, _ *OdooInstance) (admission.Warnings, error) {
	return nil, nil
}
