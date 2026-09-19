{{/* Chart name, overridable. */}}
{{- define "record-store.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* Fully qualified release name. */}}
{{- define "record-store.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "record-store.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "record-store.labels" -}}
helm.sh/chart: {{ include "record-store.chart" . }}
{{ include "record-store.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: record-store
{{- end -}}

{{- define "record-store.selectorLabels" -}}
app.kubernetes.io/name: {{ include "record-store.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* The server pods. */}}
{{- define "record-store.server.selectorLabels" -}}
{{ include "record-store.selectorLabels" . }}
app.kubernetes.io/component: server
{{- end -}}

{{- define "record-store.console.selectorLabels" -}}
{{ include "record-store.selectorLabels" . }}
app.kubernetes.io/component: console
{{- end -}}

{{- define "record-store.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "record-store.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/* Headless service backing the StatefulSet: gives every node a stable DNS
     name, which is what the consensus layer advertises to its peers. */}}
{{- define "record-store.headlessService" -}}
{{- printf "%s-headless" (include "record-store.fullname" .) -}}
{{- end -}}

{{- define "record-store.apiService" -}}
{{- printf "%s-api" (include "record-store.fullname" .) -}}
{{- end -}}

{{/* The Secret holding credentials: either one the operator already created or
     the one this chart manages. */}}
{{- define "record-store.secretName" -}}
{{- if .Values.auth.existingSecret -}}
{{- .Values.auth.existingSecret -}}
{{- else -}}
{{- printf "%s-auth" (include "record-store.fullname" .) -}}
{{- end -}}
{{- end -}}

{{/* Whether this release runs a real cluster. One replica stays standalone so a
     small installation pays nothing for consensus it does not need. */}}
{{- define "record-store.clustered" -}}
{{- if gt (int .Values.replicaCount) 1 -}}true{{- else -}}false{{- end -}}
{{- end -}}

{{/* Peer address of node 0, which every other node contacts to join. */}}
{{- define "record-store.seed" -}}
{{- printf "%s-0.%s.%s.svc.%s:%d" (include "record-store.fullname" .) (include "record-store.headlessService" .) .Release.Namespace .Values.clusterDomain (int .Values.service.ports.rpc) -}}
{{- end -}}
