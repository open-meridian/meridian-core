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
{{- if not .Values.broker.existingSecret -}}
{{- fail "broker.existingSecret is not set. The components meet on a broker, and each presents a credential from that secret; a deployment without one has components that cannot hear each other." -}}
{{- end -}}
{{- end -}}

{{/*
The Secret holding the runtime's Postgres URL.

A name the administrator gives, or one this chart makes and leaves empty for
the first-run wizard to fill. It was once required, which a fresh install
cannot satisfy: nobody has been through the wizard yet, and the wizard is what
learns the database (spec/installation-and-first-run, requirement 6).
*/}}
{{- define "meridian-runtime.migrateKey" -}}
{{- /*
  Which key in the database Secret migrates with.

  A deployment configured by its wizard holds both logins in one Secret: `url`
  is the serving role, which may not create a table, and `migrate-url` is the
  one that may. Reading `url` here migrates as the role that cannot, which is
  a permission error three layers down from the thing that chose it -- found
  on a cluster on 2026-09-22, as "permission denied for schema public".

  An administrator who named their own Secret keeps naming their own keys.
*/ -}}
{{- if .Values.migrate.key -}}
{{- .Values.migrate.key -}}
{{- else if .Values.database.existingSecret -}}
{{- .Values.database.key -}}
{{- else -}}
migrate-url
{{- end -}}
{{- end -}}

{{- define "meridian-runtime.databaseSecret" -}}
{{- .Values.database.existingSecret | default (printf "%s-database" (include "meridian-runtime.fullname" .)) -}}
{{- end -}}

{{/*
The bundled Zitadel's issuer, as Zitadel itself states it: its external
scheme, domain and port, the port left out when it is the scheme's default.
*/}}
{{- define "meridian-runtime.zitadelIssuer" -}}
{{- $c := .Values.zitadel.zitadel.configmapConfig -}}
{{- $scheme := ternary "https" "http" (ne (toString $c.ExternalSecure) "false") -}}
{{- $port := int ($c.ExternalPort | default (ternary 443 80 (eq $scheme "https"))) -}}
{{- if or (and (eq $scheme "https") (eq $port 443)) (and (eq $scheme "http") (eq $port 80)) -}}
{{- printf "%s://%s" $scheme $c.ExternalDomain -}}
{{- else -}}
{{- printf "%s://%s:%d" $scheme $c.ExternalDomain $port -}}
{{- end -}}
{{- end -}}

