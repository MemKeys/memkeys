{{/*
Expand the name of the chart.
*/}}
{{- define "memkeys.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "memkeys.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Chart name and version as used by the chart label.
*/}}
{{- define "memkeys.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels. Carries app.kubernetes.io/part-of so an operator or GitOps
controller can select every resource in the release, plus any operator hook
labels from .Values.operator.labels.
*/}}
{{- define "memkeys.labels" -}}
helm.sh/chart: {{ include "memkeys.chart" . }}
{{ include "memkeys.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/part-of: memkeys
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- with .Values.operator.labels }}
{{ toYaml . }}
{{- end }}
{{- end }}

{{/*
Selector labels (release identity, stable across profiles — safe for the
immutable Deployment .spec.selector).
*/}}
{{- define "memkeys.selectorLabels" -}}
app.kubernetes.io/name: {{ include "memkeys.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Operator hook annotations, applied to the workloads the chart renders so an
external operator/controller can key off them. No operator ships with this
chart yet — this is a forward-looking extension point.
*/}}
{{- define "memkeys.operatorAnnotations" -}}
{{- with .Values.operator.annotations }}
{{- toYaml . }}
{{- end }}
{{- end }}

{{/*
Fully qualified name of the worker workload (microservice profile).
*/}}
{{- define "memkeys.workerFullname" -}}
{{- printf "%s-worker" (include "memkeys.fullname" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Worker selector labels — release identity plus component=worker so the
worker Deployment and the server Service never select each other's pods.
*/}}
{{- define "memkeys.workerSelectorLabels" -}}
{{ include "memkeys.selectorLabels" . }}
app.kubernetes.io/component: worker
{{- end }}

{{/*
Active deployment profile, validated. single-node (default) | microservice.
*/}}
{{- define "memkeys.profile" -}}
{{- $p := .Values.profile | default "single-node" -}}
{{- if not (has $p (list "single-node" "microservice")) -}}
{{- fail (printf "memkeys: invalid profile %q (want \"single-node\" or \"microservice\")" $p) -}}
{{- end -}}
{{- $p -}}
{{- end }}

{{/*
Whether the separate worker Deployment renders. An explicit
workers.enabled bool wins; otherwise it follows the profile (on for
microservice). Emits the string "true"/"false".
*/}}
{{- define "memkeys.workersEnabled" -}}
{{- if kindIs "bool" .Values.workers.enabled -}}
{{- .Values.workers.enabled | ternary "true" "false" -}}
{{- else if eq (include "memkeys.profile" .) "microservice" -}}
true
{{- else -}}
false
{{- end -}}
{{- end }}

{{/*
Effective server replica count. An explicit .Values.replicaCount wins;
otherwise it defaults per profile — microservice → 2 (HA, drain-tolerant PDB),
single-node → 1.
*/}}
{{- define "memkeys.serverReplicas" -}}
{{- if not (kindIs "invalid" .Values.replicaCount) -}}
{{- .Values.replicaCount -}}
{{- else if eq (include "memkeys.profile" .) "microservice" -}}
2
{{- else -}}
1
{{- end -}}
{{- end }}

{{/*
Whether the PodDisruptionBudget renders. An explicit
podDisruptionBudget.enabled bool wins (the operator owns the consequences);
otherwise it renders for the microservice profile ONLY when effective server
replicas > 1 — a PDB with minAvailable: 1 on a singleton would deadlock
`kubectl drain`. Emits "true"/"false".
*/}}
{{- define "memkeys.pdbEnabled" -}}
{{- if kindIs "bool" .Values.podDisruptionBudget.enabled -}}
{{- .Values.podDisruptionBudget.enabled | ternary "true" "false" -}}
{{- else if and (eq (include "memkeys.profile" .) "microservice") (gt (int (include "memkeys.serverReplicas" .)) 1) -}}
true
{{- else -}}
false
{{- end -}}
{{- end }}

{{/*
ServiceAccount name
*/}}
{{- define "memkeys.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "memkeys.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Name of the Secret that holds MEMKEYS_DATABASE_URL.
*/}}
{{- define "memkeys.databaseSecretName" -}}
{{- if .Values.database.existingSecret }}
{{- .Values.database.existingSecret }}
{{- else }}
{{- printf "%s-db" (include "memkeys.fullname" .) }}
{{- end }}
{{- end }}

{{/*
The image reference, defaulting the tag to the chart appVersion.
*/}}
{{- define "memkeys.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end }}
