package controller

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"testing"

	utilruntime "k8s.io/apimachinery/pkg/util/runtime"
	"k8s.io/client-go/kubernetes/scheme"
	"k8s.io/client-go/rest"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/envtest"

	memkeysv1alpha1 "github.com/MemKeys/memkeys/operator/api/v1alpha1"
)

// These are envtest-backed integration tests: a real kube-apiserver + etcd are
// started by setup-envtest (KUBEBUILDER_ASSETS). The reconciler is driven
// directly against that apiserver. This is the operator e2e smoke wired into CI
// (ISI-6085 acceptance).

var (
	cfg       *rest.Config
	k8sClient client.Client
	testEnv   *envtest.Environment
	ctx       context.Context
	cancel    context.CancelFunc
)

func TestMain(m *testing.M) {
	ctx, cancel = context.WithCancel(context.Background())

	utilruntime.Must(memkeysv1alpha1.AddToScheme(scheme.Scheme))

	testEnv = &envtest.Environment{
		CRDDirectoryPaths:     []string{filepath.Join("..", "..", "config", "crd", "bases")},
		ErrorIfCRDPathMissing: true,
	}

	code := func() int {
		var err error
		cfg, err = testEnv.Start()
		if err != nil {
			fmt.Fprintf(os.Stderr, "failed to start envtest (is KUBEBUILDER_ASSETS set? run `make envtest`): %v\n", err)
			return 1
		}
		defer func() { _ = testEnv.Stop() }()

		k8sClient, err = client.New(cfg, client.Options{Scheme: scheme.Scheme})
		if err != nil {
			fmt.Fprintf(os.Stderr, "failed to build client: %v\n", err)
			return 1
		}
		return m.Run()
	}()

	cancel()
	os.Exit(code)
}
