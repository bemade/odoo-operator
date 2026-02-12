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

package controller

import (
	"context"
	"fmt"

	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"
)

// OperatorDefaults holds cluster-specific configuration injected into the
// OdooInstance controller at startup via command-line flags. These values are
// written into OdooInstance spec fields the first time the resource is
// reconciled, making the spec self-describing for all other controllers.
type OperatorDefaults struct {
	OdooImage     string // --default-odoo-image
	StorageClass  string // --default-storage-class
	StorageSize   string // --default-storage-size
	IngressClass  string // --default-ingress-class
	IngressIssuer string // --default-ingress-issuer
	// Complex types parsed from JSON flags.
	Resources   *corev1.ResourceRequirements // --default-resources (JSON)
	Affinity    *corev1.Affinity             // --default-affinity (JSON)
	Tolerations []corev1.Toleration          // --default-tolerations (JSON)
}

// ptr returns a pointer to the given value. Useful for setting optional struct fields.
func ptr[T any](v T) *T { return &v }

// sanitiseUID converts a UUID string into a safe database name component by
// replacing any non-lowercase-alphanumeric characters with underscores.
func sanitiseUID(uid string) string {
	b := make([]byte, len(uid))
	for i, c := range uid {
		if (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') {
			b[i] = byte(c)
		} else {
			b[i] = '_'
		}
	}
	return string(b)
}

// scaleDeployment patches the replica count on the named Deployment using a raw
// merge patch to avoid fetching the full object.
func scaleDeployment(ctx context.Context, c client.Client, name, namespace string, replicas int32) error {
	patch := []byte(fmt.Sprintf(`{"spec":{"replicas":%d}}`, replicas))
	target := &unstructured.Unstructured{}
	target.SetGroupVersionKind(schema.GroupVersionKind{Group: "apps", Version: "v1", Kind: "Deployment"})
	target.SetName(name)
	target.SetNamespace(namespace)
	return c.Patch(ctx, target, client.RawPatch(types.MergePatchType, patch))
}
