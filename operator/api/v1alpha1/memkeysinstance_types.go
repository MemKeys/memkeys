package v1alpha1

import (
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// Profile selects which workloads the operator reconciles for an instance.
// It mirrors the M10a Helm chart's `profile` value so the operator and chart
// stay coherent (ISI-6084).
// +kubebuilder:validation:Enum=single-node;microservice
type Profile string

const (
	// ProfileSingleNode runs one all-in-one memkeys-server Deployment
	// (writer + inline projection). No worker Deployment, no PDB.
	ProfileSingleNode Profile = "single-node"
	// ProfileMicroservice runs the server Deployment plus a separate
	// async-projection worker Deployment and a PodDisruptionBudget.
	ProfileMicroservice Profile = "microservice"
)

// ImageSpec describes the memkeys-server container image. The same image runs
// both the server and the worker role; the role is selected via command/args.
type ImageSpec struct {
	// Repository is the image repository (without tag).
	// +kubebuilder:default="ghcr.io/memkeys/memkeys"
	Repository string `json:"repository,omitempty"`
	// Tag is the image tag. Empty defaults to the operator's pinned appVersion.
	Tag string `json:"tag,omitempty"`
	// PullPolicy is the container image pull policy.
	// +kubebuilder:validation:Enum=Always;IfNotPresent;Never
	// +kubebuilder:default=IfNotPresent
	PullPolicy corev1.PullPolicy `json:"pullPolicy,omitempty"`
}

// DatabaseSpec wires the required MEMKEYS_DATABASE_URL into the workloads.
// Provide exactly one of URL (operator renders a Secret) or ExistingSecret
// (reference a pre-provisioned Secret).
type DatabaseSpec struct {
	// URL is the Postgres connection string. When set, the operator renders a
	// Secret holding it. Prefer ExistingSecret in production so the URL is not
	// stored in the MemKeysInstance object.
	URL string `json:"url,omitempty"`
	// ExistingSecret references a pre-provisioned Secret holding the URL.
	ExistingSecret string `json:"existingSecret,omitempty"`
	// ExistingSecretKey is the key within the Secret holding the URL.
	// +kubebuilder:default="MEMKEYS_DATABASE_URL"
	ExistingSecretKey string `json:"existingSecretKey,omitempty"`
}

// OTelSpec configures OpenTelemetry export (M4). An empty Endpoint selects the
// no-op exporters.
type OTelSpec struct {
	// Endpoint is the OTLP/gRPC collector endpoint. Empty → no-op exporters.
	Endpoint string `json:"endpoint,omitempty"`
	// DeploymentEnv drives sampling + resource attributes (dev|staging|prod).
	// +kubebuilder:default="dev"
	DeploymentEnv string `json:"deploymentEnv,omitempty"`
	// ServiceNamespace sets the service.namespace resource attribute.
	ServiceNamespace string `json:"serviceNamespace,omitempty"`
	// TracesSamplerRatio optionally overrides head-sampling ratio (prod).
	TracesSamplerRatio string `json:"tracesSamplerRatio,omitempty"`
}

// WorkersSpec configures the async-projection worker Deployment (microservice
// profile). Enabled defaults to the profile (on for microservice, off for
// single-node) unless set explicitly.
type WorkersSpec struct {
	// Enabled forces the worker Deployment on/off regardless of profile.
	// Nil follows the profile.
	Enabled *bool `json:"enabled,omitempty"`
	// Replicas is the worker replica count.
	// +kubebuilder:default=1
	// +kubebuilder:validation:Minimum=0
	Replicas int32 `json:"replicas,omitempty"`
	// Command overrides the container entrypoint to select the worker role.
	Command []string `json:"command,omitempty"`
	// Args are appended to the worker container command.
	Args []string `json:"args,omitempty"`
	// Resources are the worker container resource requirements.
	Resources corev1.ResourceRequirements `json:"resources,omitempty"`
}

// PodDisruptionBudgetSpec configures the server PDB (microservice profile).
type PodDisruptionBudgetSpec struct {
	// Enabled forces the PDB on/off regardless of profile. Nil follows the
	// profile. A PDB is only rendered when effective server replicas > 1 — a
	// minAvailable:1 PDB on a single replica would block `kubectl drain`.
	Enabled *bool `json:"enabled,omitempty"`
	// MinAvailable is the PDB minAvailable (integer).
	// +kubebuilder:default=1
	// +kubebuilder:validation:Minimum=0
	MinAvailable int32 `json:"minAvailable,omitempty"`
}

// MemKeysInstanceSpec defines the desired state of a MemKeys instance.
type MemKeysInstanceSpec struct {
	// Profile selects which workloads are reconciled.
	// +kubebuilder:default=single-node
	Profile Profile `json:"profile,omitempty"`

	// Replicas is the server replica count. Nil → profile default
	// (1 for single-node, 2 for microservice so the PDB tolerates a drain).
	// +kubebuilder:validation:Minimum=0
	Replicas *int32 `json:"replicas,omitempty"`

	// Image describes the memkeys-server container image.
	Image ImageSpec `json:"image,omitempty"`

	// Database wires the required MEMKEYS_DATABASE_URL.
	// +kubebuilder:validation:Required
	Database DatabaseSpec `json:"database"`

	// OTel configures OpenTelemetry export.
	OTel OTelSpec `json:"otel,omitempty"`

	// Workers configures the async-projection worker Deployment.
	Workers WorkersSpec `json:"workers,omitempty"`

	// PodDisruptionBudget configures the server PDB.
	PodDisruptionBudget PodDisruptionBudgetSpec `json:"podDisruptionBudget,omitempty"`

	// ServicePort is the port the Service exposes and the container listens on.
	// +kubebuilder:default=8080
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=65535
	ServicePort int32 `json:"servicePort,omitempty"`

	// Resources are the server container resource requirements.
	Resources corev1.ResourceRequirements `json:"resources,omitempty"`

	// ImagePullSecrets are referenced by both the server and worker pods.
	ImagePullSecrets []corev1.LocalObjectReference `json:"imagePullSecrets,omitempty"`
}

// Condition type constants for MemKeysInstance status.
const (
	// ConditionReady is True when the instance's server (and, for the
	// microservice profile, workers) are fully available.
	ConditionReady = "Ready"
	// ConditionProgressing is True while the operator is still rolling out
	// desired state (replicas not yet all available).
	ConditionProgressing = "Progressing"
	// ConditionDegraded is True when a managed Deployment reports a failure
	// (e.g. image pull / crash loop surfaced as ReplicaFailure).
	ConditionDegraded = "Degraded"
)

// MemKeysInstanceStatus defines the observed state of a MemKeys instance.
type MemKeysInstanceStatus struct {
	// ObservedGeneration is the .metadata.generation the status reflects.
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`

	// Conditions follow the standard metav1.Condition semantics
	// (Ready / Progressing / Degraded).
	// +listType=map
	// +listMapKey=type
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// ServerReplicas is the desired server replica count.
	ServerReplicas int32 `json:"serverReplicas,omitempty"`
	// ReadyServerReplicas is the number of available server replicas.
	ReadyServerReplicas int32 `json:"readyServerReplicas,omitempty"`
	// WorkerReplicas is the desired worker replica count (0 when disabled).
	WorkerReplicas int32 `json:"workerReplicas,omitempty"`
	// ReadyWorkerReplicas is the number of available worker replicas.
	ReadyWorkerReplicas int32 `json:"readyWorkerReplicas,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=mki,categories=memkeys
// +kubebuilder:printcolumn:name="Profile",type=string,JSONPath=`.spec.profile`
// +kubebuilder:printcolumn:name="Ready",type=string,JSONPath=`.status.conditions[?(@.type=="Ready")].status`
// +kubebuilder:printcolumn:name="Server",type=string,JSONPath=`.status.readyServerReplicas`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// MemKeysInstance is the Schema for the memkeysinstances API.
type MemKeysInstance struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   MemKeysInstanceSpec   `json:"spec,omitempty"`
	Status MemKeysInstanceStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// MemKeysInstanceList contains a list of MemKeysInstance.
type MemKeysInstanceList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []MemKeysInstance `json:"items"`
}

func init() {
	SchemeBuilder.Register(&MemKeysInstance{}, &MemKeysInstanceList{})
}
