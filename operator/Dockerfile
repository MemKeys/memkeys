# Multi-stage build for the MemKeys operator manager. Produces a static,
# distroless/nonroot image (UID 65532) — same hardening posture as the
# memkeys-server image (ISI-6097) and the Helm chart securityContext (M10a).
FROM golang:1.23 AS build
WORKDIR /workspace

# Cache deps first.
COPY go.mod go.mod
COPY go.sum go.sum
RUN go mod download

COPY cmd/ cmd/
COPY api/ api/
COPY internal/ internal/

# CGO off → static binary that runs on distroless/static.
RUN CGO_ENABLED=0 GOOS=linux go build -trimpath -o manager ./cmd

FROM gcr.io/distroless/static:nonroot
WORKDIR /
COPY --from=build /workspace/manager /manager
USER 65532:65532
ENTRYPOINT ["/manager"]
