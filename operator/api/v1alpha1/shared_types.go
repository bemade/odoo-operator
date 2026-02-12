/*
Copyright 2026 Marc Durepos, Bemade Inc.

This file is part of odoo-operator.

odoo-operator is free software: you can redistribute it and/or modify it under
the terms of the GNU Lesser General Public License as published by the Free
Software Foundation, either version 3 of the License, or (at your option) any
later version.

odoo-operator is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
FOR A PARTICULAR PURPOSE. See the GNU Lesser General Public License for more
details.

You should have received a copy of the GNU Lesser General Public License along
with odoo-operator. If not, see <https://www.gnu.org/licenses/>.
*/

package v1alpha1

import corev1 "k8s.io/api/core/v1"

// BackupFormat specifies the format of the backup artifact.
// +kubebuilder:validation:Enum=zip;sql;dump
type BackupFormat string

const (
	// BackupFormatZip creates an Odoo-format zip archive including the filestore.
	BackupFormatZip BackupFormat = "zip"
	// BackupFormatSQL creates a plain-text SQL dump via pg_dump.
	BackupFormatSQL BackupFormat = "sql"
	// BackupFormatDump creates a PostgreSQL custom-format dump via pg_dump.
	BackupFormatDump BackupFormat = "dump"
)

// S3Config holds connection details for an S3-compatible object store.
type S3Config struct {
	// bucket is the S3 bucket name.
	Bucket string `json:"bucket"`

	// objectKey is the object key (path) within the bucket.
	ObjectKey string `json:"objectKey"`

	// endpoint is the S3-compatible endpoint URL (e.g. "https://s3.example.com").
	Endpoint string `json:"endpoint"`

	// region is the optional S3 region.
	// +optional
	Region string `json:"region,omitempty"`

	// insecure disables TLS certificate verification.
	// +optional
	// +kubebuilder:default=false
	Insecure bool `json:"insecure,omitempty"`

	// s3CredentialsSecretRef references a Secret with accessKey and secretKey fields.
	// +optional
	CredentialsSecretRef *corev1.SecretReference `json:"s3CredentialsSecretRef,omitempty"`
}
