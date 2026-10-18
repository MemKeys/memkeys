package controller

import (
	"context"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	policyv1 "k8s.io/api/policy/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
	logf "sigs.k8s.io/controller-runtime/pkg/log"

	memkeysv1alpha1 "github.com/MemKeys/memkeys/operator/api/v1alpha1"
)

// MemKeysInstanceReconciler reconciles a MemKeysInstance object into the server
// and (microservice profile) worker Deployments, a Service, an optional
// operator-rendered DB Secret, and an optional PodDisruptionBudget.
type MemKeysInstanceReconciler struct {
	client.Client
	Scheme *runtime.Scheme
}

// +kubebuilder:rbac:groups=memkeys.io,resources=memkeysinstances,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=memkeys.io,resources=memkeysinstances/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=memkeys.io,resources=memkeysinstances/finalizers,verbs=update
// +kubebuilder:rbac:groups=apps,resources=deployments,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups="",resources=services;secrets,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=policy,resources=poddisruptionbudgets,verbs=get;list;watch;create;update;patch;delete

// Reconcile drives actual cluster state toward the MemKeysInstance spec.
func (r *MemKeysInstanceReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := logf.FromContext(ctx)

	var mki memkeysv1alpha1.MemKeysInstance
	if err := r.Get(ctx, req.NamespacedName, &mki); err != nil {
		// Not found → owned objects are garbage-collected via owner references.
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	// Fail closed on an unusable DB config rather than rolling out pods that
	// reference a Secret that does not exist.
	if mki.Spec.Database.URL == "" && mki.Spec.Database.ExistingSecret == "" {
		return ctrl.Result{}, r.markDegraded(ctx, &mki, "DatabaseUnconfigured",
			"spec.database requires either url or existingSecret")
	}

	// The operator renders the DB Secret only when an inline url is given and no
	// existing Secret is referenced (mirrors charts/memkeys/templates/secret.yaml).
	if mki.Spec.Database.URL != "" && mki.Spec.Database.ExistingSecret == "" {
		if err := r.apply(ctx, &mki, buildDBSecret(&mki)); err != nil {
			return ctrl.Result{}, err
		}
	}

	if err := r.apply(ctx, &mki, buildServerDeployment(&mki)); err != nil {
		return ctrl.Result{}, err
	}
	if err := r.apply(ctx, &mki, buildService(&mki)); err != nil {
		return ctrl.Result{}, err
	}

	// Worker Deployment: reconcile when enabled, prune when disabled.
	if workersEnabled(&mki) {
		if err := r.apply(ctx, &mki, buildWorkerDeployment(&mki)); err != nil {
			return ctrl.Result{}, err
		}
	} else if err := r.prune(ctx, &appsv1.Deployment{
		ObjectMeta: metav1.ObjectMeta{Name: workerName(&mki), Namespace: mki.Namespace},
	}); err != nil {
		return ctrl.Result{}, err
	}

	// PodDisruptionBudget: only with >1 server replica, else prune.
	if pdbEnabled(&mki) {
		if err := r.apply(ctx, &mki, buildPDB(&mki)); err != nil {
			return ctrl.Result{}, err
		}
	} else if err := r.prune(ctx, &policyv1.PodDisruptionBudget{
		ObjectMeta: metav1.ObjectMeta{Name: serverName(&mki), Namespace: mki.Namespace},
	}); err != nil {
		return ctrl.Result{}, err
	}

	log.V(1).Info("reconciled instance",
		"profile", effectiveProfile(&mki), "serverReplicas", serverReplicas(&mki),
		"workers", workersEnabled(&mki), "pdb", pdbEnabled(&mki))

	return ctrl.Result{}, r.updateStatus(ctx, &mki)
}

// apply performs a server-side apply of a desired object, setting the
// MemKeysInstance as the controller owner. SSA avoids the read-modify-write
// churn (and reconcile loops) that overwriting apiserver-defaulted fields would
// cause.
func (r *MemKeysInstanceReconciler) apply(ctx context.Context, owner *memkeysv1alpha1.MemKeysInstance, obj client.Object) error {
	if err := controllerutil.SetControllerReference(owner, obj, r.Scheme); err != nil {
		return err
	}
	return r.Patch(ctx, obj, client.Apply, client.ForceOwnership, client.FieldOwner(fieldManager))
}

// prune deletes an object that should no longer exist (profile/knob change),
// ignoring a missing object.
func (r *MemKeysInstanceReconciler) prune(ctx context.Context, obj client.Object) error {
	return client.IgnoreNotFound(r.Delete(ctx, obj))
}

// updateStatus recomputes the Ready/Progressing/Degraded conditions from the
// real Deployment availability — never fabricated.
func (r *MemKeysInstanceReconciler) updateStatus(ctx context.Context, mki *memkeysv1alpha1.MemKeysInstance) error {
	desiredServer := serverReplicas(mki)

	var serverDep appsv1.Deployment
	serverFound := true
	if err := r.Get(ctx, types.NamespacedName{Name: serverName(mki), Namespace: mki.Namespace}, &serverDep); err != nil {
		if apierrors.IsNotFound(err) {
			serverFound = false
		} else {
			return err
		}
	}

	var readyServer int32
	serverFailed, progressDeadline := false, false
	if serverFound {
		readyServer = serverDep.Status.AvailableReplicas
		for _, c := range serverDep.Status.Conditions {
			if c.Type == appsv1.DeploymentReplicaFailure && c.Status == corev1.ConditionTrue {
				serverFailed = true
			}
			if c.Type == appsv1.DeploymentProgressing && c.Status == corev1.ConditionFalse && c.Reason == "ProgressDeadlineExceeded" {
				progressDeadline = true
			}
		}
	}

	workersOn := workersEnabled(mki)
	var desiredWorker, readyWorker int32
	workerFailed := false
	if workersOn {
		desiredWorker = workerReplicas(mki)
		var workerDep appsv1.Deployment
		switch err := r.Get(ctx, types.NamespacedName{Name: workerName(mki), Namespace: mki.Namespace}, &workerDep); {
		case err == nil:
			readyWorker = workerDep.Status.AvailableReplicas
			for _, c := range workerDep.Status.Conditions {
				if c.Type == appsv1.DeploymentReplicaFailure && c.Status == corev1.ConditionTrue {
					workerFailed = true
				}
			}
		case apierrors.IsNotFound(err):
			// just created; treated as not-yet-ready below
		default:
			return err
		}
	}

	degraded := serverFailed || workerFailed || progressDeadline
	serverSatisfied := serverFound && readyServer >= desiredServer
	workerSatisfied := !workersOn || readyWorker >= desiredWorker
	ready := !degraded && serverSatisfied && workerSatisfied

	mki.Status.ObservedGeneration = mki.Generation
	mki.Status.ServerReplicas = desiredServer
	mki.Status.ReadyServerReplicas = readyServer
	mki.Status.WorkerReplicas = desiredWorker
	mki.Status.ReadyWorkerReplicas = readyWorker

	gen := mki.Generation
	if ready {
		setCondition(mki, memkeysv1alpha1.ConditionReady, metav1.ConditionTrue, "AllReplicasAvailable",
			"server and workers are fully available", gen)
		setCondition(mki, memkeysv1alpha1.ConditionProgressing, metav1.ConditionFalse, "RolloutComplete",
			"desired state reached", gen)
		setCondition(mki, memkeysv1alpha1.ConditionDegraded, metav1.ConditionFalse, "AsExpected",
			"no Deployment failures reported", gen)
	} else if degraded {
		reason, msg := "DeploymentReplicaFailure", "a managed Deployment reports a replica failure"
		if progressDeadline {
			reason, msg = "ProgressDeadlineExceeded", "a managed Deployment exceeded its progress deadline"
		}
		setCondition(mki, memkeysv1alpha1.ConditionReady, metav1.ConditionFalse, reason, msg, gen)
		setCondition(mki, memkeysv1alpha1.ConditionProgressing, metav1.ConditionFalse, reason, msg, gen)
		setCondition(mki, memkeysv1alpha1.ConditionDegraded, metav1.ConditionTrue, reason, msg, gen)
	} else {
		setCondition(mki, memkeysv1alpha1.ConditionReady, metav1.ConditionFalse, "WaitingForReplicas",
			"waiting for server/worker replicas to become available", gen)
		setCondition(mki, memkeysv1alpha1.ConditionProgressing, metav1.ConditionTrue, "RollingOut",
			"rolling out desired state", gen)
		setCondition(mki, memkeysv1alpha1.ConditionDegraded, metav1.ConditionFalse, "AsExpected",
			"no Deployment failures reported", gen)
	}

	return r.Status().Update(ctx, mki)
}

// markDegraded sets a terminal Degraded condition (e.g. misconfiguration) and
// persists status. No requeue — a spec fix bumps the generation and re-triggers.
func (r *MemKeysInstanceReconciler) markDegraded(ctx context.Context, mki *memkeysv1alpha1.MemKeysInstance, reason, msg string) error {
	gen := mki.Generation
	mki.Status.ObservedGeneration = gen
	setCondition(mki, memkeysv1alpha1.ConditionReady, metav1.ConditionFalse, reason, msg, gen)
	setCondition(mki, memkeysv1alpha1.ConditionProgressing, metav1.ConditionFalse, reason, msg, gen)
	setCondition(mki, memkeysv1alpha1.ConditionDegraded, metav1.ConditionTrue, reason, msg, gen)
	return r.Status().Update(ctx, mki)
}

func setCondition(mki *memkeysv1alpha1.MemKeysInstance, condType string, status metav1.ConditionStatus, reason, msg string, gen int64) {
	meta.SetStatusCondition(&mki.Status.Conditions, metav1.Condition{
		Type:               condType,
		Status:             status,
		Reason:             reason,
		Message:            msg,
		ObservedGeneration: gen,
	})
}

// SetupWithManager wires the controller: it owns the Deployments / Service /
// Secret / PDB it creates, so their status changes re-trigger reconcile.
func (r *MemKeysInstanceReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&memkeysv1alpha1.MemKeysInstance{}).
		Owns(&appsv1.Deployment{}).
		Owns(&corev1.Service{}).
		Owns(&corev1.Secret{}).
		Owns(&policyv1.PodDisruptionBudget{}).
		Complete(r)
}
