{{- define "meridian-runtime.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "meridian-runtime.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name (include "meridian-runtime.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "meridian-runtime.labels" -}}
app.kubernetes.io/name: {{ include "meridian-runtime.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/*
Refuse rather than render something that starts and cannot work.

Each of these has no default worth guessing. A replica with no deployment
identifier cannot sign; one with no key cannot either; one with no database has
nowhere to keep what it is told. Installing and then crash-looping would tell
the operator the same thing far less clearly.
*/}}
{{- define "meridian-runtime.require" -}}
{{- if not .Values.deployment.id -}}
{{- fail "deployment.id is not set. Register this replica's public key with the platform; the identifier comes back from that." -}}
{{- end -}}
{{- if and (not .Values.key.existingSecret) (not .Values.key.generate) -}}
{{- fail "neither key.existingSecret nor key.generate is set. Set key.generate=true to have the replica make its own key inside the cluster, and read the public half from the key job's log; or create a secret yourself and name it here." -}}
{{- end -}}
{{- if and .Values.key.existingSecret .Values.key.generate -}}
{{- fail "key.existingSecret and key.generate are both set, and they mean opposite things: one supplies a key, the other makes one. Pick." -}}
{{- end -}}
{{- if not .Values.database.existingSecret -}}
{{- fail "database.existingSecret is not set. The replica needs a Postgres URL, in a secret rather than in your values." -}}
{{- end -}}
{{- end -}}
