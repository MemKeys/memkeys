package controller

import (
	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	policyv1 "k8s.io/api/policy/v1"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/util/intstr"
	"strconv"

	memkeysv1alpha1 "github.com/MemKeys/memkeys/operator/api/v1alpha1"
)

// The operator and the M10a Helm chart (ISI-6084) render the same workloads.
// These builders mirror charts/memkeys/templates/* so an instance reconciled by
// the operator is byte-coherent with a `helm install` of the same profile.

const (
	fieldManager    = "memkeys-operator"
	labelManagedBy  = "memkeys-operator"
	labelPartOf     = "memkeys"
	labelAppName    = "memkeys"
	componentServer = "server"
	componentWorker = "worker"

	defaultImageRepo = "ghcr.io/memkeys/memkeys"
	defaultImageTag  = "latest"

	// roleWorker is the MEMKEYS_ROLE value selecting the projection-only role
	// in the memkeys-server binary (health + reconciler, no memory transport).
	roleWorker = "worker"
)

func ptrInt32(i int32) *int32 { return &i }

// --- names ------------------------------------------------------------------

func serverName(mki *memkeysv1alpha1.MemKeysInstance) string   { return mki.Name }
func workerName(mki *memkeysv1alpha1.MemKeysInstance) string   { return mki.Name + "-worker" }
func dbSecretName(mki *memkeysv1alpha1.MemKeysInstance) string { return mki.Name + "-db" }

// --- effective values (profile-aware, mirrors _helpers.tpl) -----------------

func effectiveProfile(mki *memkeysv1alpha1.MemKeysInstance) memkeysv1alpha1.Profile {
	if mki.Spec.Profile == "" {
		return memkeysv1alpha1.ProfileSingleNode
	}
	return mki.Spec.Profile
}

func isMicroservice(mki *memkeysv1alpha1.MemKeysInstance) bool {
	return effectiveProfile(mki) == memkeysv1alpha1.ProfileMicroservice
}

// serverReplicas resolves the server replica count: an explicit spec.replicas
// wins; otherwise the profile default (microservice → 2 for drain-tolerance,
// single-node → 1).
func serverReplicas(mki *memkeysv1alpha1.MemKeysInstance) int32 {
	if mki.Spec.Replicas != nil {
		return *mki.Spec.Replicas
	}
	if isMicroservice(mki) {
		return 2
	}
	return 1
}

// workersEnabled resolves whether the worker Deployment is reconciled: an
// explicit spec.workers.enabled wins; otherwise it follows the profile.
func workersEnabled(mki *memkeysv1alpha1.MemKeysInstance) bool {
	if mki.Spec.Workers.Enabled != nil {
		return *mki.Spec.Workers.Enabled
	}
	return isMicroservice(mki)
}

// pdbEnabled resolves whether the server PDB is reconciled. It follows the
// profile (or an explicit override) AND requires >1 server replica — a
// minAvailable:1 PDB on a single replica would make `kubectl drain` block
// forever (same guard as the chart).
func pdbEnabled(mki *memkeysv1alpha1.MemKeysInstance) bool {
	base := isMicroservice(mki)
	if mki.Spec.PodDisruptionBudget.Enabled != nil {
		base = *mki.Spec.PodDisruptionBudget.Enabled
	}
	return base && serverReplicas(mki) > 1
}

func pdbMinAvailable(mki *memkeysv1alpha1.MemKeysInstance) int32 {
	minAvail := int32(1)
	if mki.Spec.PodDisruptionBudget.MinAvailable > 0 {
		minAvail = mki.Spec.PodDisruptionBudget.MinAvailable
	}
	// Drain-deadlock guard: minAvailable >= replicas leaves allowedDisruptions
	// at 0, so `kubectl drain` blocks forever — clamp to replicas-1. pdbEnabled
	// only renders a PDB with replicas > 1, so the clamp target is positive
	// whenever the value is used.
	if max := serverReplicas(mki) - 1; max > 0 && minAvail > max {
		return max
	}
	return minAvail
}

func workerReplicas(mki *memkeysv1alpha1.MemKeysInstance) int32 {
	if mki.Spec.Workers.Replicas > 0 {
		return mki.Spec.Workers.Replicas
	}
	return 1
}

func servicePort(mki *memkeysv1alpha1.MemKeysInstance) int32 {
	if mki.Spec.ServicePort > 0 {
		return mki.Spec.ServicePort
	}
	return 8080
}

// memoryTransportEnabled resolves whether the /memory/* transport is exposed:
// an explicit spec.memoryTransport.enabled wins; nil → enabled (the product
// API is the point of the deployment, ISI-6233).
func memoryTransportEnabled(mki *memkeysv1alpha1.MemKeysInstance) bool {
	if mki.Spec.MemoryTransport.Enabled != nil {
		return *mki.Spec.MemoryTransport.Enabled
	}
	return true
}

// memoryTransportPort is the /memory/* listen + Service port. The server has
// its own default (3000) — 0 (unset) mirrors it.
func memoryTransportPort(mki *memkeysv1alpha1.MemKeysInstance) int32 {
	if mki.Spec.MemoryTransport.Port > 0 {
		return mki.Spec.MemoryTransport.Port
	}
	return 3000
}

// memoryTransportCredentialed reports whether the transport may legally bind
// off loopback: a run-token verifier secret (preferred) or the explicit
// dev-only insecure opt-in. Mirrors the server's own bind gate — without one
// of these the server refuses to start, so the reconciler must fail closed.
func memoryTransportCredentialed(mki *memkeysv1alpha1.MemKeysInstance) bool {
	return mki.Spec.MemoryTransport.RunTokenSecret != nil || mki.Spec.MemoryTransport.AllowInsecure
}

// memoryTransportValid is the fail-closed gate: enabled AND credentialed AND
// no port collision with the health surface (the two listeners are distinct;
// equal ports would crash the process on the second bind).
func memoryTransportValid(mki *memkeysv1alpha1.MemKeysInstance) bool {
	return !memoryTransportEnabled(mki) ||
		(memoryTransportCredentialed(mki) && memoryTransportPort(mki) != servicePort(mki))
}

// memoryTransportEnv renders the exposure env for the SERVER container only
// (the worker role never binds the transport). The chart's deployment.yaml
// renders the same keys.
func memoryTransportEnv(mki *memkeysv1alpha1.MemKeysInstance) []corev1.EnvVar {
	if !memoryTransportEnabled(mki) {
		return nil
	}
	env := []corev1.EnvVar{
		{Name: "MEMKEYS_MEMORY_HTTP_PORT", Value: strconv.Itoa(int(memoryTransportPort(mki)))},
		{Name: "MEMKEYS_HTTP_HOST", Value: "0.0.0.0"},
	}
	if s := mki.Spec.MemoryTransport.RunTokenSecret; s != nil {
		key := s.Key
		if key == "" {
			key = "MEMKEYS_RUN_TOKEN_SECRET"
		}
		env = append(env, corev1.EnvVar{
			Name: "MEMKEYS_RUN_TOKEN_SECRET",
			ValueFrom: &corev1.EnvVarSource{
				SecretKeyRef: &corev1.SecretKeySelector{
					LocalObjectReference: corev1.LocalObjectReference{Name: s.Name},
					Key:                  key,
				},
			},
		})
	} else {
		env = append(env, corev1.EnvVar{Name: "MEMKEYS_ALLOW_INSECURE", Value: "1"})
	}
	return env
}

// --- labels -----------------------------------------------------------------

func commonLabels(mki *memkeysv1alpha1.MemKeysInstance) map[string]string {
	return map[string]string{
		"app.kubernetes.io/name":       labelAppName,
		"app.kubernetes.io/instance":   mki.Name,
		"app.kubernetes.io/part-of":    labelPartOf,
		"app.kubernetes.io/managed-by": labelManagedBy,
	}
}

// selectorLabels are the immutable Deployment/Service selector. Component is
// included so the server and worker selectors never overlap.
func selectorLabels(mki *memkeysv1alpha1.MemKeysInstance, component string) map[string]string {
	return map[string]string{
		"app.kubernetes.io/name":      labelAppName,
		"app.kubernetes.io/instance":  mki.Name,
		"app.kubernetes.io/component": component,
	}
}

func podLabels(mki *memkeysv1alpha1.MemKeysInstance, component string) map[string]string {
	l := commonLabels(mki)
	l["app.kubernetes.io/component"] = component
	return l
}

// --- shared pod building blocks ---------------------------------------------

func imageRef(img memkeysv1alpha1.ImageSpec) string {
	repo := img.Repository
	if repo == "" {
		repo = defaultImageRepo
	}
	tag := img.Tag
	if tag == "" {
		tag = defaultImageTag
	}
	return repo + ":" + tag
}

func pullPolicy(img memkeysv1alpha1.ImageSpec) corev1.PullPolicy {
	if img.PullPolicy == "" {
		return corev1.PullIfNotPresent
	}
	return img.PullPolicy
}

// dbSecretRef resolves the Secret name + key that holds MEMKEYS_DATABASE_URL.
// When spec.database.existingSecret is set it is referenced directly; otherwise
// the operator-rendered Secret (<name>-db) is used.
func dbSecretRef(mki *memkeysv1alpha1.MemKeysInstance) (name, key string) {
	key = mki.Spec.Database.ExistingSecretKey
	if key == "" {
		key = "MEMKEYS_DATABASE_URL"
	}
	if mki.Spec.Database.ExistingSecret != "" {
		return mki.Spec.Database.ExistingSecret, key
	}
	return dbSecretName(mki), key
}

// containerEnv wires MEMKEYS_DATABASE_URL (from the Secret) plus the OTel env
// the server reads (mirrors charts/memkeys/templates/configmap.yaml, except the
// DB URL which is a secretKeyRef rather than a ConfigMap value).
func containerEnv(mki *memkeysv1alpha1.MemKeysInstance) []corev1.EnvVar {
	secretName, secretKey := dbSecretRef(mki)
	env := []corev1.EnvVar{{
		Name: "MEMKEYS_DATABASE_URL",
		ValueFrom: &corev1.EnvVarSource{
			SecretKeyRef: &corev1.SecretKeySelector{
				LocalObjectReference: corev1.LocalObjectReference{Name: secretName},
				Key:                  secretKey,
			},
		},
	}}
	// Keep the health surface's listen port pinned to spec.servicePort so the
	// Service port, container port, and MEMKEYS_HTTP_PORT always agree (the
	// server defaults to 8080 — a non-8080 servicePort without this env left
	// /readyz bound to 8080 and the pod never Ready).
	env = append(env, corev1.EnvVar{Name: "MEMKEYS_HTTP_PORT", Value: strconv.Itoa(int(servicePort(mki)))})
	deployEnv := mki.Spec.OTel.DeploymentEnv
	if deployEnv == "" {
		deployEnv = "dev"
	}
	env = append(env, corev1.EnvVar{Name: "MEMKEYS_DEPLOYMENT_ENV", Value: deployEnv})
	if v := mki.Spec.OTel.Endpoint; v != "" {
		env = append(env, corev1.EnvVar{Name: "OTEL_EXPORTER_OTLP_ENDPOINT", Value: v})
	}
	if v := mki.Spec.OTel.ServiceNamespace; v != "" {
		env = append(env, corev1.EnvVar{Name: "MEMKEYS_SERVICE_NAMESPACE", Value: v})
	}
	if v := mki.Spec.OTel.TracesSamplerRatio; v != "" {
		env = append(env, corev1.EnvVar{Name: "MEMKEYS_TRACES_SAMPLER_RATIO", Value: v})
	}
	return env
}

// defaultResources mirrors the chart's conservative requests/limits; used when
// the instance leaves resources unset.
func resourcesOrDefault(r corev1.ResourceRequirements) corev1.ResourceRequirements {
	if len(r.Requests) > 0 || len(r.Limits) > 0 {
		return r
	}
	return corev1.ResourceRequirements{
		Requests: corev1.ResourceList{
			corev1.ResourceCPU:    resource.MustParse("50m"),
			corev1.ResourceMemory: resource.MustParse("64Mi"),
		},
		Limits: corev1.ResourceList{
			corev1.ResourceCPU:    resource.MustParse("500m"),
			corev1.ResourceMemory: resource.MustParse("256Mi"),
		},
	}
}

// podSecurityContext / containerSecurityContext match the chart defaults for a
// distroless/nonroot runtime image (UID/GID 65532, read-only root fs, all caps
// dropped). Not configurable in v1alpha1 — a hardened floor by construction.
func podSecurityContext() *corev1.PodSecurityContext {
	nonRoot := true
	id := int64(65532)
	return &corev1.PodSecurityContext{
		RunAsNonRoot:   &nonRoot,
		RunAsUser:      &id,
		RunAsGroup:     &id,
		FSGroup:        &id,
		SeccompProfile: &corev1.SeccompProfile{Type: corev1.SeccompProfileTypeRuntimeDefault},
	}
}

func containerSecurityContext() *corev1.SecurityContext {
	noEsc := false
	roRoot := true
	nonRoot := true
	id := int64(65532)
	return &corev1.SecurityContext{
		AllowPrivilegeEscalation: &noEsc,
		ReadOnlyRootFilesystem:   &roRoot,
		RunAsNonRoot:             &nonRoot,
		RunAsUser:                &id,
		Capabilities:             &corev1.Capabilities{Drop: []corev1.Capability{"ALL"}},
	}
}

// --- object builders --------------------------------------------------------

func buildDBSecret(mki *memkeysv1alpha1.MemKeysInstance) *corev1.Secret {
	_, key := dbSecretRef(mki)
	return &corev1.Secret{
		TypeMeta: metav1.TypeMeta{APIVersion: "v1", Kind: "Secret"},
		ObjectMeta: metav1.ObjectMeta{
			Name:      dbSecretName(mki),
			Namespace: mki.Namespace,
			Labels:    commonLabels(mki),
		},
		Type:       corev1.SecretTypeOpaque,
		StringData: map[string]string{key: mki.Spec.Database.URL},
	}
}

func buildService(mki *memkeysv1alpha1.MemKeysInstance) *corev1.Service {
	port := servicePort(mki)
	ports := []corev1.ServicePort{{
		Name:       "http",
		Port:       port,
		TargetPort: intstr.FromString("http"),
		Protocol:   corev1.ProtocolTCP,
	}}
	// The /memory/* transport Service port — the port clients target. The
	// health port above stays probe-only (ISI-6233).
	if memoryTransportEnabled(mki) {
		ports = append(ports, corev1.ServicePort{
			Name:       "memory",
			Port:       memoryTransportPort(mki),
			TargetPort: intstr.FromString("memory"),
			Protocol:   corev1.ProtocolTCP,
		})
	}
	return &corev1.Service{
		TypeMeta: metav1.TypeMeta{APIVersion: "v1", Kind: "Service"},
		ObjectMeta: metav1.ObjectMeta{
			Name:      serverName(mki),
			Namespace: mki.Namespace,
			Labels:    commonLabels(mki),
		},
		Spec: corev1.ServiceSpec{
			Type:     corev1.ServiceTypeClusterIP,
			Selector: selectorLabels(mki, componentServer),
			Ports:    ports,
		},
	}
}

func buildServerDeployment(mki *memkeysv1alpha1.MemKeysInstance) *appsv1.Deployment {
	port := servicePort(mki)
	containerPorts := []corev1.ContainerPort{{
		Name:          "http",
		ContainerPort: port,
		Protocol:      corev1.ProtocolTCP,
	}}
	if memoryTransportEnabled(mki) {
		containerPorts = append(containerPorts, corev1.ContainerPort{
			Name:          "memory",
			ContainerPort: memoryTransportPort(mki),
			Protocol:      corev1.ProtocolTCP,
		})
	}
	container := corev1.Container{
		Name:            "memkeys",
		Image:           imageRef(mki.Spec.Image),
		ImagePullPolicy: pullPolicy(mki.Spec.Image),
		SecurityContext: containerSecurityContext(),
		Ports:           containerPorts,
		Env:             append(containerEnv(mki), memoryTransportEnv(mki)...),
		Resources:       resourcesOrDefault(mki.Spec.Resources),
		LivenessProbe: &corev1.Probe{
			ProbeHandler: corev1.ProbeHandler{HTTPGet: &corev1.HTTPGetAction{
				Path: "/healthz", Port: intstr.FromString("http"),
			}},
			InitialDelaySeconds: 5, PeriodSeconds: 10, TimeoutSeconds: 2, FailureThreshold: 3,
		},
		ReadinessProbe: &corev1.Probe{
			ProbeHandler: corev1.ProbeHandler{HTTPGet: &corev1.HTTPGetAction{
				Path: "/readyz", Port: intstr.FromString("http"),
			}},
			InitialDelaySeconds: 3, PeriodSeconds: 10, TimeoutSeconds: 2, FailureThreshold: 3,
		},
	}
	return &appsv1.Deployment{
		TypeMeta: metav1.TypeMeta{APIVersion: "apps/v1", Kind: "Deployment"},
		ObjectMeta: metav1.ObjectMeta{
			Name:      serverName(mki),
			Namespace: mki.Namespace,
			Labels:    commonLabels(mki),
		},
		Spec: appsv1.DeploymentSpec{
			Replicas: ptrInt32(serverReplicas(mki)),
			Selector: &metav1.LabelSelector{MatchLabels: selectorLabels(mki, componentServer)},
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{Labels: podLabels(mki, componentServer)},
				Spec: corev1.PodSpec{
					ImagePullSecrets:             mki.Spec.ImagePullSecrets,
					SecurityContext:              podSecurityContext(),
					AutomountServiceAccountToken: ptrBool(false),
					Containers:                   []corev1.Container{container},
				},
			},
		},
	}
}

func buildWorkerDeployment(mki *memkeysv1alpha1.MemKeysInstance) *appsv1.Deployment {
	workerRes := resourcesOrDefault(mki.Spec.Workers.Resources)
	// The worker role: projection-only (health surface + reconciler tick, no
	// memory transport) — honoured by the memkeys-server binary. Mirrors the
	// chart's workers-deployment.yaml env; spec.workers.command/args remain
	// user-overridable entrypoints.
	workerEnv := append(containerEnv(mki), corev1.EnvVar{Name: "MEMKEYS_ROLE", Value: string(roleWorker)})
	container := corev1.Container{
		Name:            "memkeys-worker",
		Image:           imageRef(mki.Spec.Image),
		ImagePullPolicy: pullPolicy(mki.Spec.Image),
		SecurityContext: containerSecurityContext(),
		Command:         mki.Spec.Workers.Command,
		Args:            mki.Spec.Workers.Args,
		Env:             workerEnv,
		Resources:       workerRes,
	}
	return &appsv1.Deployment{
		TypeMeta: metav1.TypeMeta{APIVersion: "apps/v1", Kind: "Deployment"},
		ObjectMeta: metav1.ObjectMeta{
			Name:      workerName(mki),
			Namespace: mki.Namespace,
			Labels:    podLabels(mki, componentWorker),
		},
		Spec: appsv1.DeploymentSpec{
			Replicas: ptrInt32(workerReplicas(mki)),
			Selector: &metav1.LabelSelector{MatchLabels: selectorLabels(mki, componentWorker)},
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{Labels: podLabels(mki, componentWorker)},
				Spec: corev1.PodSpec{
					ImagePullSecrets:             mki.Spec.ImagePullSecrets,
					SecurityContext:              podSecurityContext(),
					AutomountServiceAccountToken: ptrBool(false),
					Containers:                   []corev1.Container{container},
				},
			},
		},
	}
}

func buildPDB(mki *memkeysv1alpha1.MemKeysInstance) *policyv1.PodDisruptionBudget {
	minAvail := intstr.FromInt32(pdbMinAvailable(mki))
	return &policyv1.PodDisruptionBudget{
		TypeMeta: metav1.TypeMeta{APIVersion: "policy/v1", Kind: "PodDisruptionBudget"},
		ObjectMeta: metav1.ObjectMeta{
			Name:      serverName(mki),
			Namespace: mki.Namespace,
			Labels:    podLabels(mki, componentServer),
		},
		Spec: policyv1.PodDisruptionBudgetSpec{
			MinAvailable: &minAvail,
			Selector:     &metav1.LabelSelector{MatchLabels: selectorLabels(mki, componentServer)},
		},
	}
}

func ptrBool(b bool) *bool { return &b }
