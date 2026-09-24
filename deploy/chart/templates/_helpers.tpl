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
{{- fail "key.generate is off and key.existingSecret names nothing, so this deployment has no way to get a key. Leave key.generate on to have the conductor make its own inside the cluster, or name a secret holding one." -}}
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

{{- define "meridian-runtime.generatedKey" -}}
{{- /*
  Whether this deployment makes its own key.

  Generating is the default, because it is how an install gets a key with
  nobody handling one. A named secret wins: an administrator who has a key
  already is not asking for a second, and the two are not a contradiction to
  refuse but a preference to honour.
*/ -}}
{{- if .Values.key.existingSecret -}}
{{- else -}}
{{- .Values.key.generate -}}
{{- end -}}
{{- end -}}

{{- define "meridian-runtime.brokerSecret" -}}
{{- /*
  Where a component reads its own broker credential.

  An administrator's own broker is named in values; otherwise this chart
  brings one and holds the credentials it made, which is what leaves an
  install with no Secret for anybody to write by hand.
*/ -}}
{{- .Values.broker.existingSecret | default (printf "%s-broker" (include "meridian-runtime.fullname" .)) -}}
{{- end -}}

{{- define "meridian-runtime.runtimeBrokerKey" -}}
{{- /*
  Which key in the broker Secret the stores and the conductor read.

  They share one credential, because the registry gives them one set of
  topics: the generator writes it as `runtime`. An administrator naming their
  own Secret keeps naming their own keys.
*/ -}}
{{- if .Values.broker.existingSecret -}}
{{- .key -}}
{{- else -}}
runtime
{{- end -}}
{{- end -}}

{{- define "meridian-runtime.databaseSecret" -}}
{{- .Values.database.existingSecret | default (printf "%s-database" (include "meridian-runtime.fullname" .)) -}}
{{- end -}}
