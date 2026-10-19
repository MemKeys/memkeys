# Multi-stage build for the MemKeys operator manager. Produces a static,
# distroless/nonroot image (UID 65532) — same hardening posture as the
# memkeys-server image (ISI-6097) and the Helm chart securityContext (M10a).
#
# Multi-arch (linux/amd64 + linux/arm64): the build stage pins to the
# builder's native arch ($BUILDPLATFORM) and cross-compiles with GOARCH=
# $TARGETARCH. CGO is off, so the Go toolchain cross-compiles natively —
# no QEMU emulation of the compiler, which keeps arm64 builds fast (ISI-6304).
FROM --platform=$BUILDPLATFORM golang:1.23 AS build
ARG TARGETOS
ARG TARGETARCH
WORKDIR /workspace

# Cache deps first.
COPY go.mod go.mod
COPY go.sum go.sum
RUN go mod download

COPY cmd/ cmd/
COPY api/ api/
COPY internal/ internal/

# CGO off → static binary that runs on distroless/static.
RUN CGO_ENABLED=0 GOOS=${TARGETOS:-linux} GOARCH=${TARGETARCH:-amd64} \
    go build -trimpath -o manager ./cmd

FROM gcr.io/distroless/static:nonroot
WORKDIR /
COPY --from=build /workspace/manager /manager
USER 65532:65532
ENTRYPOINT ["/manager"]
