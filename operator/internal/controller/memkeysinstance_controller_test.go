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
