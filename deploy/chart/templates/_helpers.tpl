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
{{ include "meridian-runtime.unversionedLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end -}}

{{/*
  The labels without the release's version, for the pod template of anything
  whose image does not move with the release: the database this chart brings,
  and the registry. A pod template is what a rollout compares, so a version
  label there restarts the pod on every upgrade, whatever else changed. The
  database restarted under the migration Job that way, the Job failed, and the
  release never came up (CI run 36631603926). Each resource's own metadata
  keeps the version, where it restarts nothing. chart-check refuses it back.
*/}}
{{- define "meridian-runtime.unversionedLabels" -}}
app.kubernetes.io/name: {{ include "meridian-runtime.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
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

{{/*
Readiness for a component with no port: the street store, the instrument
store, the conductor and the launcher.

Each waits for what it needs -- its database, its schema, the broker --
rather than exiting, and writes this file once it serves
(meridian_runtime::Ready). The probe looks for it, so a rollout waits for a
component that is still waiting, where before it counted the pod ready the
moment it started and stopped the old one (task
kernel/upgrading-a-deployment-in-place). A file rather than a port, because a
port would be a way into a component that nothing outside its pod needs to
reach. Four pieces, because a container's env, probe and mounts and its pod's
volumes are four places.
*/}}
{{- define "meridian-runtime.readyEnv" -}}
- name: MERIDIAN_READY_FILE
  value: /run/meridian-ready/serving
{{- end -}}

{{- define "meridian-runtime.readyProbe" -}}
readinessProbe:
  exec:
    command: ["test", "-e", "/run/meridian-ready/serving"]
  periodSeconds: 5
{{- end -}}

{{- define "meridian-runtime.readyMount" -}}
- name: ready
  mountPath: /run/meridian-ready
{{- end -}}

{{- define "meridian-runtime.readyVolume" -}}
- name: ready
  emptyDir:
    medium: Memory
    sizeLimit: 1Mi
{{- end -}}

{{/*
  The edge roles, comma-joined: the roles whose plugins keep an external
  party's raw records and alone may own storage for them (decisions/028,
  ruled point 1 and its amendment for `reporting`; meridian-design's
  matrix/boundaries/roles.yaml marks the same seven `edge: true`). The
  launcher is given this list, and the admission policy on plugin pods reads
  it, so the chart says it once.
*/}}
{{- define "meridian-runtime.edgeRoles" -}}
ccm,custody,dgm,match,reporting,servicing,settlement
{{- end -}}

{{/*
  "true" when a plugin holding `roles` (a list) is given its instance's
  storage: the chart gives edge plugins storage, and it holds an edge role.
  Takes a dict: top (the chart), roles.
*/}}
{{- define "meridian-runtime.storedAtTheEdge" -}}
{{- $edge := splitList "," (include "meridian-runtime.edgeRoles" .top) -}}
{{- $held := false -}}
{{- range .roles -}}
{{- if has . $edge -}}{{- $held = true -}}{{- end -}}
{{- end -}}
{{- if and .top.Values.pluginStorage.enabled $held -}}true{{- end -}}
{{- end -}}

{{/*
  An edge plugin instance's storage (decisions/028): one claim per instance,
  `<fullname>-storage-<instance>`, made by the deployment and mounted by
  that instance's pod alone. Never removed with the plugin: the chart keeps
  what it rendered (`helm.sh/resource-policy: keep`), and the launcher has no
  right to delete a claim. Removing it is an administrator's act.

  Takes a dict: top (the chart), instance, and launched with plugin (the
  plugin's name, a placeholder in the launcher's copy) for the claim the
  launcher makes, which it alone may mount again, for the same plugin.
*/}}
{{- define "meridian-runtime.pluginStorage" -}}
{{- $top := .top -}}
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: {{ include "meridian-runtime.fullname" $top }}-storage-{{ .instance }}
  labels:
    meridian.dev/component: plugin-storage
    meridian.dev/instance: {{ .instance | quote }}
    {{- if .launched }}
    meridian.dev/launched: "true"
    meridian.dev/plugin: {{ .plugin | quote }}
    {{- end }}
    {{- include "meridian-runtime.labels" $top | nindent 4 }}
  {{- if not .launched }}
  annotations:
    helm.sh/resource-policy: keep
  {{- end }}
spec:
  accessModes: [ReadWriteOnce]
  {{- with $top.Values.pluginStorage.storageClassName }}
  storageClassName: {{ . | quote }}
  {{- end }}
  resources:
    requests:
      storage: {{ $top.Values.pluginStorage.size | quote }}
{{- end -}}
