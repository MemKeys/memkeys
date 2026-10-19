package controller

import (
	"testing"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	policyv1 "k8s.io/api/policy/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes/scheme"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"

	memkeysv1alpha1 "github.com/MemKeys/memkeys/operator/api/v1alpha1"
)

func newReconciler() *MemKeysInstanceReconciler {
	return &MemKeysInstanceReconciler{Client: k8sClient, Scheme: scheme.Scheme}
}

func newNamespace(t *testing.T) string {
	t.Helper()
	ns := &corev1.Namespace{ObjectMeta: metav1.ObjectMeta{GenerateName: "mki-test-"}}
	if err := k8sClient.Create(ctx, ns); err != nil {
		t.Fatalf("create namespace: %v", err)
	}
	t.Cleanup(func() { _ = k8sClient.Delete(ctx, ns) })
	return ns.Name
}

func createInstance(t *testing.T, ns string, mutate func(*memkeysv1alpha1.MemKeysInstance)) *memkeysv1alpha1.MemKeysInstance {
	t.Helper()
	mki := &memkeysv1alpha1.MemKeysInstance{
		ObjectMeta: metav1.ObjectMeta{Name: "sample", Namespace: ns},
		Spec: memkeysv1alpha1.MemKeysInstanceSpec{
			Database: memkeysv1alpha1.DatabaseSpec{URL: "postgres://u:p@db:5432/memkeys"},
			// The memory transport is exposed by default (ISI-6233) and its
			// gate is fail-closed, so the realistic default test spec wires
			// a run-token secret; the unauthenticated case has its own test.
			MemoryTransport: memkeysv1alpha1.MemoryTransportSpec{
				RunTokenSecret: &memkeysv1alpha1.RunTokenSecretSpec{Name: "run-token"},
			},
		},
	}
	mutate(mki)
	if err := k8sClient.Create(ctx, mki); err != nil {
		t.Fatalf("create instance: %v", err)
	}
	return mki
}

func reconcile(t *testing.T, r *MemKeysInstanceReconciler, name, ns string) {
	t.Helper()
	if _, err := r.Reconcile(ctx, ctrl.Request{NamespacedName: client.ObjectKey{Name: name, Namespace: ns}}); err != nil {
		t.Fatalf("reconcile: %v", err)
	}
}

func getDeployment(t *testing.T, name, ns string) (*appsv1.Deployment, bool) {
	t.Helper()
	var d appsv1.Deployment
	err := k8sClient.Get(ctx, client.ObjectKey{Name: name, Namespace: ns}, &d)
	if apierrors.IsNotFound(err) {
		return nil, false
	}
	if err != nil {
		t.Fatalf("get deployment %s: %v", name, err)
	}
	return &d, true
}

func getInstance(t *testing.T, name, ns string) *memkeysv1alpha1.MemKeysInstance {
	t.Helper()
	var mki memkeysv1alpha1.MemKeysInstance
	if err := k8sClient.Get(ctx, client.ObjectKey{Name: name, Namespace: ns}, &mki); err != nil {
		t.Fatalf("get instance: %v", err)
	}
	return &mki
}

func condStatus(mki *memkeysv1alpha1.MemKeysInstance, condType string) metav1.ConditionStatus {
	c := meta.FindStatusCondition(mki.Status.Conditions, condType)
	if c == nil {
		return metav1.ConditionUnknown
	}
	return c.Status
}

// markDeploymentAvailable sets a Deployment's status to fully available so the
// reconciler can observe Ready (envtest has no kubelet/Deployment controller).
func markDeploymentAvailable(t *testing.T, name, ns string, replicas int32) {
	t.Helper()
	d, ok := getDeployment(t, name, ns)
	if !ok {
		t.Fatalf("deployment %s not found", name)
	}
	d.Status.Replicas = replicas
	d.Status.ReadyReplicas = replicas
	d.Status.AvailableReplicas = replicas
	d.Status.UpdatedReplicas = replicas
	d.Status.ObservedGeneration = d.Generation
	if err := k8sClient.Status().Update(ctx, d); err != nil {
		t.Fatalf("update deployment status: %v", err)
	}
}

func TestSingleNodeProfile(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		m.Spec.Profile = memkeysv1alpha1.ProfileSingleNode
	})
	reconcile(t, r, mki.Name, ns)

	srv, ok := getDeployment(t, mki.Name, ns)
	if !ok {
		t.Fatal("server Deployment not created")
	}
	if got := *srv.Spec.Replicas; got != 1 {
		t.Errorf("server replicas = %d, want 1", got)
	}
	// The operator-rendered DB Secret should exist (inline url, no existingSecret).
	var sec corev1.Secret
	if err := k8sClient.Get(ctx, client.ObjectKey{Name: mki.Name + "-db", Namespace: ns}, &sec); err != nil {
		t.Errorf("db Secret not created: %v", err)
	}
	// No worker Deployment, no PDB for single-node.
	if _, ok := getDeployment(t, mki.Name+"-worker", ns); ok {
		t.Error("worker Deployment should not exist for single-node")
	}
	var pdb policyv1.PodDisruptionBudget
	if err := k8sClient.Get(ctx, client.ObjectKey{Name: mki.Name, Namespace: ns}, &pdb); !apierrors.IsNotFound(err) {
		t.Errorf("PDB should not exist for single-node, got err=%v", err)
	}
	// Owner reference points back at the instance.
	if len(srv.OwnerReferences) != 1 || srv.OwnerReferences[0].Kind != "MemKeysInstance" {
		t.Errorf("server Deployment missing controller owner reference: %+v", srv.OwnerReferences)
	}

	out := getInstance(t, mki.Name, ns)
	if condStatus(out, memkeysv1alpha1.ConditionProgressing) != metav1.ConditionTrue {
		t.Errorf("Progressing = %s, want True", condStatus(out, memkeysv1alpha1.ConditionProgressing))
	}
	if condStatus(out, memkeysv1alpha1.ConditionReady) != metav1.ConditionFalse {
		t.Errorf("Ready = %s, want False (no replicas available yet)", condStatus(out, memkeysv1alpha1.ConditionReady))
	}
	if out.Status.ObservedGeneration != out.Generation {
		t.Errorf("observedGeneration = %d, want %d", out.Status.ObservedGeneration, out.Generation)
	}
}

func TestMicroserviceProfileBecomesReady(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		m.Spec.Profile = memkeysv1alpha1.ProfileMicroservice
	})
	reconcile(t, r, mki.Name, ns)

	srv, ok := getDeployment(t, mki.Name, ns)
	if !ok {
		t.Fatal("server Deployment not created")
	}
	if got := *srv.Spec.Replicas; got != 2 {
		t.Errorf("server replicas = %d, want 2 (microservice default)", got)
	}
	if _, ok := getDeployment(t, mki.Name+"-worker", ns); !ok {
		t.Fatal("worker Deployment not created for microservice")
	}
	var pdb policyv1.PodDisruptionBudget
	if err := k8sClient.Get(ctx, client.ObjectKey{Name: mki.Name, Namespace: ns}, &pdb); err != nil {
		t.Fatalf("PDB not created for microservice: %v", err)
	}

	// Simulate rollout completion, then reconcile: conditions must flip to Ready.
	markDeploymentAvailable(t, mki.Name, ns, 2)
	markDeploymentAvailable(t, mki.Name+"-worker", ns, 1)
	reconcile(t, r, mki.Name, ns)

	out := getInstance(t, mki.Name, ns)
	if condStatus(out, memkeysv1alpha1.ConditionReady) != metav1.ConditionTrue {
		t.Errorf("Ready = %s, want True after replicas available", condStatus(out, memkeysv1alpha1.ConditionReady))
	}
	if condStatus(out, memkeysv1alpha1.ConditionProgressing) != metav1.ConditionFalse {
		t.Errorf("Progressing = %s, want False when Ready", condStatus(out, memkeysv1alpha1.ConditionProgressing))
	}
	if out.Status.ReadyServerReplicas != 2 {
		t.Errorf("readyServerReplicas = %d, want 2", out.Status.ReadyServerReplicas)
	}
}

func TestDatabaseUnconfiguredIsDegraded(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		m.Spec.Database = memkeysv1alpha1.DatabaseSpec{} // neither url nor existingSecret
	})
	reconcile(t, r, mki.Name, ns)

	if _, ok := getDeployment(t, mki.Name, ns); ok {
		t.Error("server Deployment should not be created without a DB config")
	}
	out := getInstance(t, mki.Name, ns)
	if condStatus(out, memkeysv1alpha1.ConditionDegraded) != metav1.ConditionTrue {
		t.Errorf("Degraded = %s, want True", condStatus(out, memkeysv1alpha1.ConditionDegraded))
	}
}

func TestExistingSecretReferenced(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		m.Spec.Database = memkeysv1alpha1.DatabaseSpec{
			ExistingSecret:    "my-db-secret",
			ExistingSecretKey: "DATABASE_URL",
		}
	})
	reconcile(t, r, mki.Name, ns)

	// Operator must NOT render its own Secret when an existing one is referenced.
	var sec corev1.Secret
	if err := k8sClient.Get(ctx, client.ObjectKey{Name: mki.Name + "-db", Namespace: ns}, &sec); !apierrors.IsNotFound(err) {
		t.Errorf("operator should not render a Secret when existingSecret is set, got err=%v", err)
	}
	srv, ok := getDeployment(t, mki.Name, ns)
	if !ok {
		t.Fatal("server Deployment not created")
	}
	env := srv.Spec.Template.Spec.Containers[0].Env
	var ref *corev1.SecretKeySelector
	for i := range env {
		if env[i].Name == "MEMKEYS_DATABASE_URL" && env[i].ValueFrom != nil {
			ref = env[i].ValueFrom.SecretKeyRef
		}
	}
	if ref == nil {
		t.Fatal("MEMKEYS_DATABASE_URL secretKeyRef not wired")
	}
	if ref.Name != "my-db-secret" || ref.Key != "DATABASE_URL" {
		t.Errorf("secretKeyRef = %s/%s, want my-db-secret/DATABASE_URL", ref.Name, ref.Key)
	}
}

func TestProfileSwitchPrunesWorkerAndPDB(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		m.Spec.Profile = memkeysv1alpha1.ProfileMicroservice
	})
	reconcile(t, r, mki.Name, ns)
	if _, ok := getDeployment(t, mki.Name+"-worker", ns); !ok {
		t.Fatal("worker Deployment should exist for microservice")
	}

	// Switch to single-node; worker + PDB must be pruned.
	cur := getInstance(t, mki.Name, ns)
	cur.Spec.Profile = memkeysv1alpha1.ProfileSingleNode
	if err := k8sClient.Update(ctx, cur); err != nil {
		t.Fatalf("update instance: %v", err)
	}
	reconcile(t, r, mki.Name, ns)

	if _, ok := getDeployment(t, mki.Name+"-worker", ns); ok {
		t.Error("worker Deployment should be pruned after switching to single-node")
	}
	var pdb policyv1.PodDisruptionBudget
	if err := k8sClient.Get(ctx, client.ObjectKey{Name: mki.Name, Namespace: ns}, &pdb); !apierrors.IsNotFound(err) {
		t.Errorf("PDB should be pruned after switching to single-node, got err=%v", err)
	}
}

func TestUrlToExistingSecretSwitchPrunesDBSecret(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		m.Spec.Database = memkeysv1alpha1.DatabaseSpec{URL: "postgres://u:p@db:5432/memkeys"}
	})
	reconcile(t, r, mki.Name, ns)

	// Inline url → the operator-rendered Secret exists.
	var sec corev1.Secret
	if err := k8sClient.Get(ctx, client.ObjectKey{Name: mki.Name + "-db", Namespace: ns}, &sec); err != nil {
		t.Fatalf("operator-rendered db Secret should exist with inline url: %v", err)
	}

	// Flip to an existingSecret reference: the rendered Secret must be pruned
	// and the Deployment must point at the referenced Secret.
	cur := getInstance(t, mki.Name, ns)
	cur.Spec.Database = memkeysv1alpha1.DatabaseSpec{ExistingSecret: "my-db-secret"}
	if err := k8sClient.Update(ctx, cur); err != nil {
		t.Fatalf("update instance: %v", err)
	}
	reconcile(t, r, mki.Name, ns)

	if err := k8sClient.Get(ctx, client.ObjectKey{Name: mki.Name + "-db", Namespace: ns}, &sec); !apierrors.IsNotFound(err) {
		t.Errorf("operator-rendered db Secret should be pruned after switching to existingSecret, got err=%v", err)
	}
	srv, ok := getDeployment(t, mki.Name, ns)
	if !ok {
		t.Fatal("server Deployment not found")
	}
	for _, e := range srv.Spec.Template.Spec.Containers[0].Env {
		if e.Name == "MEMKEYS_DATABASE_URL" && e.ValueFrom != nil {
			if got := e.ValueFrom.SecretKeyRef.Name; got != "my-db-secret" {
				t.Errorf("secretKeyRef name = %s, want my-db-secret", got)
			}
		}
	}
}

func TestPDBMinAvailableClampedBelowReplicas(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	replicas := int32(3)
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		m.Spec.Profile = memkeysv1alpha1.ProfileMicroservice
		m.Spec.Replicas = &replicas
		// >= replicas → allowedDisruptions 0 → drain deadlock; must clamp to 2.
		m.Spec.PodDisruptionBudget.MinAvailable = 3
	})
	reconcile(t, r, mki.Name, ns)

	var pdb policyv1.PodDisruptionBudget
	if err := k8sClient.Get(ctx, client.ObjectKey{Name: mki.Name, Namespace: ns}, &pdb); err != nil {
		t.Fatalf("PDB not created: %v", err)
	}
	if pdb.Spec.MinAvailable == nil {
		t.Fatal("PDB minAvailable not set")
	}
	if got := pdb.Spec.MinAvailable.IntValue(); got != 2 {
		t.Errorf("PDB minAvailable = %d, want 2 (clamped below replicas=3)", got)
	}
}

func TestServicePortWiredToHTTPPortEnv(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		m.Spec.ServicePort = 9090
	})
	reconcile(t, r, mki.Name, ns)

	srv, ok := getDeployment(t, mki.Name, ns)
	if !ok {
		t.Fatal("server Deployment not created")
	}
	container := srv.Spec.Template.Spec.Containers[0]
	// Health + memory listeners (the transport is enabled by default).
	if len(container.Ports) != 2 {
		t.Errorf("containerPorts = %+v, want health 9090 + memory 3000", container.Ports)
	}
	httpPort := ""
	for _, e := range container.Env {
		if e.Name == "MEMKEYS_HTTP_PORT" {
			httpPort = e.Value
		}
	}
	if httpPort != "9090" {
		t.Errorf("MEMKEYS_HTTP_PORT = %q, want 9090 (servicePort must reach the container env)", httpPort)
	}

	var svc corev1.Service
	if err := k8sClient.Get(ctx, client.ObjectKey{Name: mki.Name, Namespace: ns}, &svc); err != nil {
		t.Fatalf("Service not created: %v", err)
	}
	if got := svc.Spec.Ports[0].Port; got != 9090 {
		t.Errorf("Service port = %d, want 9090", got)
	}
}

// TestMemoryTransportExposedViaService is the ISI-6233 acceptance: the
// /memory/* transport must be routable via the Service — its own container
// port, its own Service port, and the env that makes the server bind off
// loopback with a run-token verifier.
func TestMemoryTransportExposedViaService(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		m.Spec.MemoryTransport.Port = 3111
		m.Spec.MemoryTransport.RunTokenSecret = &memkeysv1alpha1.RunTokenSecretSpec{
			Name: "my-run-token", Key: "ISSUER",
		}
	})
	reconcile(t, r, mki.Name, ns)

	srv, ok := getDeployment(t, mki.Name, ns)
	if !ok {
		t.Fatal("server Deployment not created")
	}
	container := srv.Spec.Template.Spec.Containers[0]
	var memoryPort *corev1.ContainerPort
	for i := range container.Ports {
		if container.Ports[i].Name == "memory" {
			memoryPort = &container.Ports[i]
		}
	}
	if memoryPort == nil || memoryPort.ContainerPort != 3111 {
		t.Errorf("memory containerPort = %+v, want named port memory:3111", container.Ports)
	}

	env := map[string]*corev1.EnvVar{}
	for i := range container.Env {
		e := container.Env[i]
		env[e.Name] = &e
	}
	if v := env["MEMKEYS_MEMORY_HTTP_PORT"]; v == nil || v.Value != "3111" {
		t.Errorf("MEMKEYS_MEMORY_HTTP_PORT = %+v, want 3111", env["MEMKEYS_MEMORY_HTTP_PORT"])
	}
	if v := env["MEMKEYS_HTTP_HOST"]; v == nil || v.Value != "0.0.0.0" {
		t.Errorf("MEMKEYS_HTTP_HOST = %+v, want 0.0.0.0 (loopback default is unreachable off-pod)", env["MEMKEYS_HTTP_HOST"])
	}
	if v := env["MEMKEYS_ALLOW_INSECURE"]; v != nil {
		t.Errorf("MEMKEYS_ALLOW_INSECURE must not be set with a runTokenSecret, got %+v", v)
	}
	sec := env["MEMKEYS_RUN_TOKEN_SECRET"]
	if sec == nil || sec.ValueFrom == nil || sec.ValueFrom.SecretKeyRef == nil {
		t.Fatalf("MEMKEYS_RUN_TOKEN_SECRET secretKeyRef not wired: %+v", sec)
	}
	if ref := sec.ValueFrom.SecretKeyRef; ref.Name != "my-run-token" || ref.Key != "ISSUER" {
		t.Errorf("runToken secretKeyRef = %s/%s, want my-run-token/ISSUER", ref.Name, ref.Key)
	}

	// The Service must front the memory listener — this is the routability
	// assertion: a client targeting the Service's memory port reaches /memory/*.
	var svc corev1.Service
	if err := k8sClient.Get(ctx, client.ObjectKey{Name: mki.Name, Namespace: ns}, &svc); err != nil {
		t.Fatalf("Service not created: %v", err)
	}
	var svcMemory *corev1.ServicePort
	for i := range svc.Spec.Ports {
		if svc.Spec.Ports[i].Name == "memory" {
			svcMemory = &svc.Spec.Ports[i]
		}
	}
	if svcMemory == nil {
		t.Fatalf("Service has no memory port: %+v", svc.Spec.Ports)
	}
	if svcMemory.Port != 3111 || svcMemory.TargetPort.String() != "memory" {
		t.Errorf("Service memory port = %+v, want port 3111 targeting container port memory", svcMemory)
	}
	// Health stays routable for probes on its own port.
	if svc.Spec.Ports[0].Name != "http" || svc.Spec.Ports[0].Port != 8080 {
		t.Errorf("Service health port = %+v, want http:8080", svc.Spec.Ports[0])
	}
}

func TestMemoryTransportUnauthenticatedIsDegraded(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		m.Spec.MemoryTransport.RunTokenSecret = nil // enabled, no creds
	})
	reconcile(t, r, mki.Name, ns)

	// Fail closed: no Deployment/Service rolled out — the server would refuse
	// the non-loopback bind and crash-loop otherwise.
	if _, ok := getDeployment(t, mki.Name, ns); ok {
		t.Error("server Deployment should not be created without a memory transport credential")
	}
	var svc corev1.Service
	if err := k8sClient.Get(ctx, client.ObjectKey{Name: mki.Name, Namespace: ns}, &svc); !apierrors.IsNotFound(err) {
		t.Errorf("Service should not be created without a memory transport credential, got err=%v", err)
	}
	out := getInstance(t, mki.Name, ns)
	if condStatus(out, memkeysv1alpha1.ConditionDegraded) != metav1.ConditionTrue {
		t.Errorf("Degraded = %s, want True", condStatus(out, memkeysv1alpha1.ConditionDegraded))
	}
	if c := meta.FindStatusCondition(out.Status.Conditions, memkeysv1alpha1.ConditionDegraded); c != nil && c.Reason != "MemoryTransportUnauthenticated" {
		t.Errorf("Degraded reason = %s, want MemoryTransportUnauthenticated", c.Reason)
	}
}

func TestMemoryTransportPortCollisionIsDegraded(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		m.Spec.MemoryTransport.Port = 8080 // same as the health servicePort
	})
	reconcile(t, r, mki.Name, ns)

	if _, ok := getDeployment(t, mki.Name, ns); ok {
		t.Error("server Deployment should not be created with a health/memory port collision")
	}
	out := getInstance(t, mki.Name, ns)
	if condStatus(out, memkeysv1alpha1.ConditionDegraded) != metav1.ConditionTrue {
		t.Errorf("Degraded = %s, want True", condStatus(out, memkeysv1alpha1.ConditionDegraded))
	}
	if c := meta.FindStatusCondition(out.Status.Conditions, memkeysv1alpha1.ConditionDegraded); c != nil && c.Reason != "MemoryTransportPortCollision" {
		t.Errorf("Degraded reason = %s, want MemoryTransportPortCollision", c.Reason)
	}
}

func TestMemoryTransportDisabled(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		disabled := false
		m.Spec.MemoryTransport.Enabled = &disabled
	})
	reconcile(t, r, mki.Name, ns)

	srv, ok := getDeployment(t, mki.Name, ns)
	if !ok {
		t.Fatal("server Deployment not created")
	}
	container := srv.Spec.Template.Spec.Containers[0]
	if len(container.Ports) != 1 || container.Ports[0].Name != "http" {
		t.Errorf("containerPorts = %+v, want only the health port", container.Ports)
	}
	for _, e := range container.Env {
		if e.Name == "MEMKEYS_HTTP_HOST" || e.Name == "MEMKEYS_MEMORY_HTTP_PORT" ||
			e.Name == "MEMKEYS_RUN_TOKEN_SECRET" || e.Name == "MEMKEYS_ALLOW_INSECURE" {
			t.Errorf("memory transport env %s must not be set when disabled", e.Name)
		}
	}

	var svc corev1.Service
	if err := k8sClient.Get(ctx, client.ObjectKey{Name: mki.Name, Namespace: ns}, &svc); err != nil {
		t.Fatalf("Service not created: %v", err)
	}
	if len(svc.Spec.Ports) != 1 || svc.Spec.Ports[0].Name != "http" {
		t.Errorf("Service ports = %+v, want only the health port", svc.Spec.Ports)
	}
}

func TestMemoryTransportInsecureOptIn(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		m.Spec.MemoryTransport.RunTokenSecret = nil
		m.Spec.MemoryTransport.AllowInsecure = true
	})
	reconcile(t, r, mki.Name, ns)

	srv, ok := getDeployment(t, mki.Name, ns)
	if !ok {
		t.Fatal("server Deployment not created")
	}
	container := srv.Spec.Template.Spec.Containers[0]
	var insecure, token string
	for _, e := range container.Env {
		switch e.Name {
		case "MEMKEYS_ALLOW_INSECURE":
			insecure = e.Value
		case "MEMKEYS_RUN_TOKEN_SECRET":
			token = "set"
		}
	}
	if insecure != "1" {
		t.Errorf("MEMKEYS_ALLOW_INSECURE = %q, want 1", insecure)
	}
	if token != "" {
		t.Error("MEMKEYS_RUN_TOKEN_SECRET must not be set in insecure mode")
	}
}

func TestWorkerDeploymentSelectsWorkerRole(t *testing.T) {
	ns := newNamespace(t)
	r := newReconciler()
	mki := createInstance(t, ns, func(m *memkeysv1alpha1.MemKeysInstance) {
		m.Spec.Profile = memkeysv1alpha1.ProfileMicroservice
	})
	reconcile(t, r, mki.Name, ns)

	w, ok := getDeployment(t, mki.Name+"-worker", ns)
	if !ok {
		t.Fatal("worker Deployment not created")
	}
	role := ""
	for _, e := range w.Spec.Template.Spec.Containers[0].Env {
		if e.Name == "MEMKEYS_ROLE" {
			role = e.Value
		}
	}
	if role != "worker" {
		t.Errorf("worker MEMKEYS_ROLE = %q, want worker", role)
	}

	srv, _ := getDeployment(t, mki.Name, ns)
	for _, e := range srv.Spec.Template.Spec.Containers[0].Env {
		if e.Name == "MEMKEYS_ROLE" {
			t.Errorf("server container must not set MEMKEYS_ROLE, got %q", e.Value)
		}
	}
}
