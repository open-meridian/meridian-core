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
{{- if not .Values.key.existingSecret -}}
{{- fail "key.existingSecret is not set. Generate a key with `docker run --rm -v $PWD/keys:/var/lib/meridian <image> public-key`, then create a secret from keys/key.pem." -}}
{{- end -}}
{{- if not .Values.database.existingSecret -}}
{{- fail "database.existingSecret is not set. The replica needs a Postgres URL, in a secret rather than in your values." -}}
{{- end -}}
{{- end -}}
