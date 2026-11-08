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
{{- /* Ruling 1074: the HA profile's conductor has a standby, and a key the
       conductor generated into a volume is mounted on one node at a time. */ -}}
{{- if and (eq (include "meridian-runtime.profile" .) "ha") (not .Values.key.existingSecret) -}}
{{- fail "resilience.profile is ha and the deployment's key is generated into a volume, which one node mounts at a time, so the conductor's standby on another node could not start (ruling 1074). Put the key in a Kubernetes Secret and name it in key.existingSecret (from Vault or cert-manager where you keep keys there), then set resilience.profile: ha." -}}
{{- end -}}

{{- end -}}

{{/*
The resilience profile (step 3b; spec/a-deployment-scales-out-and-fails-
over-by-configuration section 11, question 23; ruling 1052): `tolerant`, the
default, today's single-instance shape with the 30 s tolerations; `ha`,
every count the HA configuration needs turned on by one value, each still
overridable. Anything else is refused. Takes the chart.
*/}}
{{- define "meridian-runtime.profile" -}}
{{- $profile := (.Values.resilience | default dict).profile | default "tolerant" -}}
{{- if not (has $profile (list "tolerant" "ha")) -}}
{{- fail (printf "resilience.profile is %v; it takes tolerant (the default: one member each, healed by restart) or ha (a standby per writer, the brought database with two standbys, the broker at three; each count still overridable)." $profile) -}}
{{- end -}}
{{- $profile -}}
{{- end -}}

{{/*
A count by the profile: the value given where one is, 0 included, else the
profile's. Each count the profile sets is left unset in values.yaml, so
"not given" is told from 0. Takes a dict: top, value, tolerant, ha.
*/}}
{{- define "meridian-runtime.byProfile" -}}
{{- if kindIs "invalid" .value -}}
{{- if eq (include "meridian-runtime.profile" .top) "ha" -}}{{ .ha }}{{- else -}}{{ .tolerant }}{{- end -}}
{{- else -}}
{{- .value -}}
{{- end -}}
{{- end -}}

{{/*
The counts outside the stores' standbys, each by the profile (section 11):
the brought database's standbys (0 or 2), the broker's servers (1 or 3), its
JetStream replicas (0 or 3, only with three servers), the dashboard's copies
and the analytics coordinators and workers (1 or 2). A count CI does not test
is refused (requirement 1, ruling 1047). Each takes the chart.
*/}}
{{- define "meridian-runtime.databaseStandbys" -}}
{{- $n := include "meridian-runtime.byProfile" (dict "top" . "value" (index .Values.database.brought "standbys") "tolerant" 0 "ha" 2) -}}
{{- if not (has (toString $n) (list "0" "2")) -}}
{{- fail (printf "database.brought.standbys is %v; it takes 0 (one database, restarted on its volume) or 2 (a primary and two standbys under Patroni, one synchronous), the shapes CI tests." $n) -}}
{{- end -}}
{{- $n -}}
{{- end -}}

{{- define "meridian-runtime.brokerReplicas" -}}
{{- $n := include "meridian-runtime.byProfile" (dict "top" . "value" .Values.broker.replicas "tolerant" 1 "ha" 3) -}}
{{- if not (has (toString $n) (list "1" "3")) -}}
{{- fail (printf "broker.replicas is %v; it takes 1 or 3 (a NATS cluster keeping its majority when one server is lost), the shapes CI tests." $n) -}}
{{- end -}}
{{- $n -}}
{{- end -}}

{{- define "meridian-runtime.jetstreamReplicas" -}}
{{- $n := include "meridian-runtime.byProfile" (dict "top" . "value" (index (.Values.broker.jetstream | default dict) "replicas") "tolerant" 0 "ha" 3) -}}
{{- if not (has (toString $n) (list "0" "3")) -}}
{{- fail (printf "broker.jetstream.replicas is %v; it takes 0 (the analytics units asked again on a timeout) or 3 (their work queue kept on three servers)." $n) -}}
{{- end -}}
{{- if and (eq (toString $n) "3") (ne (include "meridian-runtime.brokerReplicas" .) "3") -}}
{{- fail "broker.jetstream.replicas is 3 and broker.replicas is not: a stream kept on three servers needs three. Set broker.replicas: 3, or broker.jetstream.replicas: 0." -}}
{{- end -}}
{{- $n -}}
{{- end -}}

{{/* A stateless set's copies: 1 or 2 by the profile, at least 1. Takes a dict: top, value. */}}
{{- define "meridian-runtime.copies" -}}
{{- $n := include "meridian-runtime.byProfile" (dict "top" .top "value" .value "tolerant" 1 "ha" 2) -}}
{{- if lt (int $n) 0 -}}{{- fail (printf "%s is %v; a count is never below 0" .what $n) -}}{{- end -}}
{{- $n -}}
{{- end -}}

{{/*
One writer per ordering domain (ruling 1060). The lake, the book, the
instrument store, the street store and the conductor's configuration store
each order their changes in one sequence per partition, taken under a lock
by the one process that writes it, and the launcher launches as one. A second
replica is a second writer over the same partitions, so a component's
replicaCount is at most 1, and above it nothing renders: a copy is added by
the value that adds one in its mode (step 3b, spec/a-deployment-scales-out-
and-fails-over-by-configuration section 1), `<component>.standby`.
*/}}
{{- define "meridian-runtime.oneWriter" -}}
{{- $count := 1 -}}
{{- if hasKey .values "replicaCount" -}}{{- $count = .values.replicaCount -}}{{- end -}}
{{- if gt (int $count) 1 -}}
{{- fail (printf "%s.replicaCount is %v; it takes 0 or 1. The %s writes as one member: a second replica is a second writer over the same partitions, which corrupts the order its change log keeps (ruling 1060). Add a warm standby with %s.standby: 1, which takes over when the writer's lease lapses, its epoch fencing the old writer (step 3b)." .component $count .what .component) -}}
{{- end -}}
{{- $_ := include "meridian-runtime.standbys" . -}}
{{- end -}}

{{/*
A component's standbys (step 3b; spec section 1, plan 5c): 0, the tolerant
default, or 1, a warm standby holding no lease that takes the writer's over
when it lapses. More are refused: a count CI does not test is not offered
(requirement 1, ruling 1047). Takes a dict: component, values.
*/}}
{{- define "meridian-runtime.standbys" -}}
{{- $standby := include "meridian-runtime.byProfile" (dict "top" .top "value" (index .values "standby") "tolerant" 0 "ha" 1) -}}
{{- if not (regexMatch "^[01]$" (toString $standby)) -}}
{{- fail (printf "%s.standby is %v; it takes 0 or 1: one warm standby per writer, which CI tests (e2e-cluster-failover). Readers come with their own value." .component $standby) -}}
{{- end -}}
{{- $standby -}}
{{- end -}}

{{/*
"true" when a component runs a standby, so its members take a lease
(coordination.k8s.io/v1, in the release's namespace) to say which writes.
Takes a dict: component, values.
*/}}
{{- define "meridian-runtime.leased" -}}
{{- if eq (include "meridian-runtime.standbys" .) "1" -}}true{{- end -}}
{{- end -}}

{{/*
How many members a component runs: none where its replicaCount is 0, else
its writer and its standby. Takes a dict: component, values.
*/}}
{{- define "meridian-runtime.memberCount" -}}
{{- $count := 1 -}}
{{- if hasKey .values "replicaCount" -}}{{- $count = .values.replicaCount -}}{{- end -}}
{{- if eq (int $count) 0 -}}0{{- else -}}{{ add 1 (include "meridian-runtime.standbys" .) }}{{- end -}}
{{- end -}}

{{/*
A member set's rollout. With a standby, a new member may start beside the
writer: it waits as a standby, the writer hands its lease over as it stops,
and the epoch fences anything it began. With none, the one writer stops
before the new one starts, as ever. Takes a dict: component, values.
*/}}
{{- define "meridian-runtime.memberStrategy" -}}
strategy:
  type: RollingUpdate
  rollingUpdate:
{{- if include "meridian-runtime.leased" . }}
    maxSurge: 1
    maxUnavailable: 0
{{- else }}
    maxSurge: 0
    maxUnavailable: 1
{{- end }}
{{- end -}}

{{/*
What a member is told of itself (step 3b): its pod's name, its zone where
the cluster labels it, and with a standby its lease, the lease's timing
(leases.renewSeconds, leases.durationSeconds) and the lag a standby is ready
under (`<component>.readyLagMs`). Takes a dict: top, component, values.
*/}}
{{- define "meridian-runtime.memberEnv" -}}
- name: MERIDIAN_POD_NAME
  valueFrom:
    fieldRef:
      fieldPath: metadata.name
- name: MERIDIAN_ZONE
  valueFrom:
    fieldRef:
      fieldPath: metadata.labels['topology.kubernetes.io/zone']
{{- /* How often its writer re-reads its record (step 3b, integrity at rest):
       `<store>.scrub.intervalDays`, 7 by default, Ceph's deep scrub; 0 for
       never. */}}
- name: MERIDIAN_SCRUB_INTERVAL_DAYS
  value: {{ dig "scrub" "intervalDays" 7 .values | toString | quote }}
{{- if include "meridian-runtime.leased" . }}
- name: MERIDIAN_LEASE_NAME
  value: {{ printf "%s-%s" (include "meridian-runtime.fullname" .top) .component | quote }}
- name: MERIDIAN_LEASE_RENEW_SECONDS
  value: {{ .top.Values.leases.renewSeconds | quote }}
- name: MERIDIAN_LEASE_DURATION_SECONDS
  value: {{ .top.Values.leases.durationSeconds | quote }}
- name: MERIDIAN_READY_LAG_MS
  value: {{ index .values "readyLagMs" | default 5000 | quote }}
{{- end }}
{{- end -}}

{{/*
Where a member's pod may run (step 3b; spec section 2, plan 5a). Every
component tolerates a node not ready or unreachable for 30 s, not
Kubernetes' 300 (question 22), in both configurations. A member set of two
spreads over nodes (and zones, where `zones` names them), the writer and
its standby repelling each other, and runs under the members' service
account, whose one right is its Leases. Takes a dict: top, component,
values, and `account` where the component has a service account of its own
that holds the lease right too.
*/}}
{{- define "meridian-runtime.memberPlacement" -}}
{{- $leased := include "meridian-runtime.leased" . -}}
{{- if not .account }}
{{- /* Said either way (step 3b part 3, found by e2e-cluster-reconfigure):
       a pod template that once named the members' account keeps it when
       the name is merely left out -- the API server's deprecated
       serviceAccount field stays -- and a member set turned down to one
       then named an account the chart had removed, and no pod was made. */}}
serviceAccountName: {{ if $leased }}{{ include "meridian-runtime.fullname" .top }}-member{{ else }}default{{ end }}
{{- end }}
{{- with .top.Values.nodeSelector }}
nodeSelector:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{ include "meridian-runtime.nodeTolerations" .top }}
{{- if .top.Values.affinity }}
affinity:
  {{- toYaml .top.Values.affinity | nindent 2 }}
{{- else if or $leased .top.Values.zones }}
affinity:
  {{- if .top.Values.zones }}
  nodeAffinity:
    requiredDuringSchedulingIgnoredDuringExecution:
      nodeSelectorTerms:
        - matchExpressions:
            - key: topology.kubernetes.io/zone
              operator: In
              values:
                {{- toYaml .top.Values.zones | nindent 16 }}
  {{- end }}
  {{- if $leased }}
  podAntiAffinity:
    preferredDuringSchedulingIgnoredDuringExecution:
      - weight: 100
        podAffinityTerm:
          topologyKey: kubernetes.io/hostname
          labelSelector:
            matchLabels:
              app.kubernetes.io/instance: {{ .top.Release.Name }}
              meridian.dev/component: {{ .component }}
  {{- end }}
{{- end }}
{{- if $leased }}
topologySpreadConstraints:
  - maxSkew: 1
    topologyKey: kubernetes.io/hostname
    whenUnsatisfiable: DoNotSchedule
    labelSelector:
      matchLabels:
        app.kubernetes.io/instance: {{ .top.Release.Name }}
        meridian.dev/component: {{ .component }}
  {{- if .top.Values.zones }}
  - maxSkew: 1
    topologyKey: topology.kubernetes.io/zone
    whenUnsatisfiable: DoNotSchedule
    labelSelector:
      matchLabels:
        app.kubernetes.io/instance: {{ .top.Release.Name }}
        meridian.dev/component: {{ .component }}
  {{- end }}
{{- end }}
{{- end -}}

{{/*
The node tolerations every stateless component carries (step 3b; spec
section 2, question 22): a pod on a node not ready or unreachable is moved
after 30 s, not Kubernetes' 300, in both configurations; and the chart's own
`tolerations` after them. The brought database is not stateless and keeps
Kubernetes' own: it waits for its node, never force-deleted. Takes the chart.
*/}}
{{- define "meridian-runtime.nodeTolerations" -}}
tolerations:
  - key: node.kubernetes.io/not-ready
    operator: Exists
    effect: NoExecute
    tolerationSeconds: 30
  - key: node.kubernetes.io/unreachable
    operator: Exists
    effect: NoExecute
    tolerationSeconds: 30
  {{- with .Values.tolerations }}
  {{- toYaml . | nindent 2 }}
  {{- end }}
{{- end -}}

{{/*
A member set's PodDisruptionBudget (step 3b, plan 5a): with a standby, a
drain never takes the writer and its standby together. Takes a dict: top,
component, values.
*/}}
{{- define "meridian-runtime.memberBudget" -}}
{{- if include "meridian-runtime.leased" . }}
---
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: {{ include "meridian-runtime.fullname" .top }}-{{ .component }}
  labels:
    meridian.dev/component: {{ .component }}
    {{- include "meridian-runtime.labels" .top | nindent 4 }}
spec:
  minAvailable: 1
  selector:
    matchLabels:
      app.kubernetes.io/name: {{ include "meridian-runtime.name" .top }}
      app.kubernetes.io/instance: {{ .top.Release.Name }}
      meridian.dev/component: {{ .component }}
{{- end }}
{{- end -}}

{{/*
A set of several copies' spread (step 3b, plan 5a): over nodes, and over
zones where `zones` names them, each copy kept from the others where it can
be; for the stateless sets (the dashboard, analytics) and the broker's and
the brought database's members. Placed under a pod's spec. Takes a dict:
top, component.
*/}}
{{- define "meridian-runtime.spread" -}}
{{- if .top.Values.affinity }}
affinity:
  {{- toYaml .top.Values.affinity | nindent 2 }}
{{- else }}
affinity:
  {{- if .top.Values.zones }}
  nodeAffinity:
    requiredDuringSchedulingIgnoredDuringExecution:
      nodeSelectorTerms:
        - matchExpressions:
            - key: topology.kubernetes.io/zone
              operator: In
              values:
                {{- toYaml .top.Values.zones | nindent 16 }}
  {{- end }}
  podAntiAffinity:
    preferredDuringSchedulingIgnoredDuringExecution:
      - weight: 100
        podAffinityTerm:
          topologyKey: kubernetes.io/hostname
          labelSelector:
            matchLabels:
              app.kubernetes.io/instance: {{ .top.Release.Name }}
              meridian.dev/component: {{ .component }}
{{- end }}
topologySpreadConstraints:
  - maxSkew: 1
    topologyKey: kubernetes.io/hostname
    whenUnsatisfiable: ScheduleAnyway
    labelSelector:
      matchLabels:
        app.kubernetes.io/instance: {{ .top.Release.Name }}
        meridian.dev/component: {{ .component }}
  {{- if .top.Values.zones }}
  - maxSkew: 1
    topologyKey: topology.kubernetes.io/zone
    whenUnsatisfiable: DoNotSchedule
    labelSelector:
      matchLabels:
        app.kubernetes.io/instance: {{ .top.Release.Name }}
        meridian.dev/component: {{ .component }}
  {{- end }}
{{- end -}}

{{/*
A set of several copies' PodDisruptionBudget: a drain never takes more than
`unavailable` of them at once (one of two copies, one of three broker or
database members). Takes a dict: top, component, unavailable.
*/}}
{{- define "meridian-runtime.budget" -}}
---
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: {{ include "meridian-runtime.fullname" .top }}-{{ .component }}
  labels:
    meridian.dev/component: {{ .component }}
    {{- include "meridian-runtime.labels" .top | nindent 4 }}
spec:
  maxUnavailable: {{ .unavailable | default 1 }}
  selector:
    matchLabels:
      app.kubernetes.io/name: {{ include "meridian-runtime.name" .top }}
      app.kubernetes.io/instance: {{ .top.Release.Name }}
      meridian.dev/component: {{ .component }}
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
confirmation,connectivity,custody,data,reporting,servicing,settlement
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
{{/*
  The deployment's archive (contract v16; values.yaml's pluginArchive): `path`
  for a volume made from a directory on the node or a claim already made,
  `bucket` for a cloud's, or nothing. The one place the rule is said, so the
  conductor, the launcher and the admission policy cannot disagree.
*/}}
{{- define "meridian-runtime.archiveKind" -}}
{{- $a := .Values.pluginArchive | default dict -}}
{{- if and (or $a.path $a.existingClaim) $a.bucket -}}
{{- fail "pluginArchive names a local archive (path or existingClaim) and a bucket; a deployment keeps its archives in one place" -}}
{{- end -}}
{{- if and $a.path $a.existingClaim -}}
{{- fail "pluginArchive names a path and an existing claim; name one" -}}
{{- end -}}
{{- if and $a.objectLock (not $a.bucket) -}}
{{- fail "pluginArchive.objectLock is set without a bucket; only a cloud bucket made with object lock can hold records that cannot be altered" -}}
{{- end -}}
{{- if and $a.bucket (not $a.serviceAccount) -}}
{{- fail "pluginArchive.bucket is set without a serviceAccount; an allowed instance reaches its bucket through a workload identity scoped to it, never a key" -}}
{{- end -}}
{{- if or $a.path $a.existingClaim -}}path{{- else if $a.bucket -}}bucket{{- end -}}
{{- end -}}

{{/* The claim a local archive is, by name. */}}
{{- define "meridian-runtime.archiveClaim" -}}
{{- $a := .Values.pluginArchive | default dict -}}
{{- if $a.existingClaim -}}{{ $a.existingClaim }}{{- else if $a.path -}}{{ include "meridian-runtime.fullname" . }}-archive{{- end -}}
{{- end -}}

{{/*
  What an allowed instance is given beside its storage, as the launcher
  merges it into the plugin's container (W8.3, W8.7): a local archive's
  directory of its own, mounted at MERIDIAN_ARCHIVE_DIR, or a bucket's prefix
  of its own in MERIDIAN_ARCHIVE_BUCKET and the account scoped to it. The
  instance a placeholder the launcher fills in once it has checked it.
*/}}
{{- define "meridian-runtime.archiveShape" -}}
{{- $kind := include "meridian-runtime.archiveKind" . -}}
{{- if eq $kind "path" }}
env:
  - name: MERIDIAN_ARCHIVE_DIR
    value: /var/lib/meridian/archive
volumeMounts:
  - name: archive
    mountPath: /var/lib/meridian/archive
    subPath: __INSTANCE__
volumes:
  - name: archive
    persistentVolumeClaim:
      claimName: {{ include "meridian-runtime.archiveClaim" . }}
{{- else if eq $kind "bucket" }}
env:
  - name: MERIDIAN_ARCHIVE_BUCKET
    value: {{ printf "%s/__INSTANCE__" (trimSuffix "/" .Values.pluginArchive.bucket) | quote }}
serviceAccountName: {{ .Values.pluginArchive.serviceAccount | quote }}
{{- end }}
{{- end -}}

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

{{/*
  Whether a backup dumps the database (backup.database; task
  kernel/a-deployment-is-backed-up-and-restored, approved 2026-10-11): `auto`
  when empty, which the backup's script reads as the database this chart
  brings included and one somebody else runs left to its provider; `true` or
  `false` for either. A boolean and its string spelling are one answer.
*/}}
{{- define "meridian-runtime.backupDatabase" -}}
{{- $said := index (.Values.backup | default dict) "database" -}}
{{- if kindIs "bool" $said -}}{{ ternary "true" "false" $said }}
{{- else if or (kindIs "invalid" $said) (eq (toString $said) "") (eq (toString $said) "auto") -}}auto
{{- else -}}{{ toString $said }}{{- end -}}
{{- end -}}

{{/*
Patroni's dynamic settings for the brought database (step 3b, section 4):
the timings set for the database tier (1048: a primary lost answered for
within 30 s -- the leader key's 20 s, Patroni's least, then a promotion),
and with standbys, one synchronous and strict (question 5), so a primary
cut off from every standby cannot acknowledge a commit on its own. With
none, neither: a database alone has nothing to wait for. Pages written
whole after a checkpoint (full_page_writes) and hint bits logged, so a
torn write is replayed and a rejoining primary rewound. Takes a dict: top,
standbys.
*/}}
{{- define "meridian-runtime.patroniDynamic" -}}
ttl: 20
loop_wait: 2
retry_timeout: 5
maximum_lag_on_failover: 1048576
synchronous_mode: {{ gt (int .standbys) 0 }}
synchronous_mode_strict: {{ gt (int .standbys) 0 }}
synchronous_node_count: 1
postgresql:
  use_pg_rewind: true
  use_slots: true
  parameters:
    wal_level: replica
    hot_standby: "on"
    wal_log_hints: "on"
    full_page_writes: "on"
    max_wal_senders: 10
    max_replication_slots: 10
    wal_keep_size: 128MB
{{- end -}}

{{/*
The brought database's WAL archive (values.yaml's backup.walArchive; step
3b, spec/a-deployment-scales-out-and-fails-over-by-configuration sections 4
and 9): "true" where it is on, once what cannot stand is refused; nothing
where it is off. Takes the chart.
*/}}
{{- define "meridian-runtime.walArchive" -}}
{{- $b := .Values.backup | default dict -}}
{{- $w := $b.walArchive | default dict -}}
{{- if $w.enabled -}}
{{- if not .Values.database.brought.enabled -}}
{{- fail "backup.walArchive is on and the database is not one this chart brings. An external database's point-in-time recovery is its provider's own (spec section 4): turn the archive on there." -}}
{{- end -}}
{{- if not (and $b.recipient (or $b.path $b.existingClaim)) -}}
{{- fail "backup.walArchive is on without backups: its base backups are taken with each backup, and the passphrase it is encrypted with travels sealed in them. Set backup.recipient and backup.path or backup.existingClaim as well." -}}
{{- end -}}
{{- $store := $w.store | default "volume" -}}
{{- if not (has $store (list "volume" "s3" "gcs" "azure")) -}}
{{- fail (printf "backup.walArchive.store is %v; it takes volume (beside the sealed backups), s3, gcs or azure." $store) -}}
{{- end -}}
{{- if not (hasPrefix "/" ($w.path | default "")) -}}
{{- fail (printf "backup.walArchive.path is %q; it is a path from the store's root, beginning with /." ($w.path | default "")) -}}
{{- end -}}
{{- if and (eq $store "volume") $b.path (gt (int (include "meridian-runtime.databaseStandbys" .)) 0) -}}
{{- fail "backup.walArchive is kept on backup.path, a directory on one node, and the database has standbys on other nodes: whichever is primary archives, so the archive would be split across nodes. Name backup.existingClaim on storage every node mounts, or an object store (backup.walArchive.store)." -}}
{{- end -}}
{{- if and (ne $store "volume") (not $w.bucket) -}}
{{- fail (printf "backup.walArchive.store is %s and backup.walArchive.bucket names none." $store) -}}
{{- end -}}
{{- if and (eq $store "s3") (not (and $w.endpoint $w.region)) -}}
{{- fail "backup.walArchive.store is s3 and its endpoint or region is not set: on AWS, s3.<region>.amazonaws.com and <region>; for a store speaking S3's API, its own." -}}
{{- end -}}
{{- if and (eq $store "azure") (not $w.existingSecret) -}}
{{- fail "backup.walArchive.store is azure and backup.walArchive.existingSecret names no Secret with the storage account's `account` and `key`." -}}
{{- end -}}
{{- if and $w.kmsKeyId (ne $store "s3") -}}
{{- fail (printf "backup.walArchive.kmsKeyId is set and the store is %s: on gcs and azure a customer-managed key is the bucket's or the storage account's own setting, made there." $store) -}}
{{- end -}}
{{- if lt (int ($w.timeoutSeconds | default 60)) 1 -}}
{{- fail "backup.walArchive.timeoutSeconds is below 1." -}}
{{- end -}}
true
{{- end -}}
{{- end -}}

{{/*
pgBackRest's configuration for the archive, as each of its three users
reads it: `database`, each member's server and archiver; `backup`, the
backup Job taking a base backup from the primary over TLS; `offline`, the
Job restoring or checking a stopped member's volume. The secrets are not
here: the passphrase and the store's credentials reach each as variables
(walArchiveEnv). Takes a dict: top, role.
*/}}
{{- define "meridian-runtime.pgbackrestConf" -}}
{{- $top := .top -}}
{{- $w := $top.Values.backup.walArchive -}}
{{- $store := $w.store | default "volume" -}}
{{- $full := include "meridian-runtime.fullname" $top -}}
[global]
repo1-type={{ ternary "posix" $store (eq $store "volume") }}
repo1-path={{ if eq $store "volume" }}/backups{{ end }}{{ $w.path }}
repo1-cipher-type=aes-256-cbc
repo1-retention-full-type=count
repo1-retention-full={{ $top.Values.backup.keep | default 14 }}
{{- if eq $store "s3" }}
repo1-s3-bucket={{ $w.bucket }}
repo1-s3-endpoint={{ $w.endpoint }}
repo1-s3-region={{ $w.region }}
{{- if not $w.existingSecret }}
repo1-s3-key-type=auto
{{- end }}
{{- with $w.kmsKeyId }}
repo1-s3-kms-key-id={{ . }}
{{- end }}
{{- else if eq $store "gcs" }}
repo1-gcs-bucket={{ $w.bucket }}
{{- with $w.endpoint }}
repo1-gcs-endpoint={{ . }}
{{- end }}
{{- if $w.existingSecret }}
repo1-gcs-key-type=service
repo1-gcs-key=/etc/pgbackrest-store/key
{{- else }}
repo1-gcs-key-type=auto
{{- end }}
{{- else if eq $store "azure" }}
repo1-azure-container={{ $w.bucket }}
{{- with $w.endpoint }}
repo1-azure-endpoint={{ . }}
{{- end }}
{{- end }}
start-fast=y
archive-timeout=120
log-level-console=info
log-level-stderr=off
log-level-file=off
lock-path=/tmp/pgbackrest
{{- if eq .role "database" }}
tls-server-address=0.0.0.0
tls-server-ca-file=/run/archive/tls/ca.crt
tls-server-cert-file=/run/archive/tls/tls.crt
tls-server-key-file=/run/archive/tls/tls.key
tls-server-auth=meridian-backup=meridian
{{- end }}

[meridian]
pg1-path=/var/lib/postgresql/data/pgdata
pg1-socket-path=/var/run/postgresql
{{- if eq .role "backup" }}
pg1-host={{ $full }}-database
pg1-host-type=tls
pg1-host-ca-file=/run/archive/tls/ca.crt
pg1-host-cert-file=/run/archive/tls/client.crt
pg1-host-key-file=/run/archive/tls/client.key
{{- end }}
{{- end -}}

{{/*
The archive's secrets as pgBackRest reads them, from variables: the
passphrase it is encrypted with, and the object store's credentials where
the values name them. Placed under a container's env. Takes the chart.
*/}}
{{- define "meridian-runtime.walArchiveEnv" -}}
{{- $w := .Values.backup.walArchive -}}
{{- $full := include "meridian-runtime.fullname" . -}}
- name: PGBACKREST_REPO1_CIPHER_PASS
  valueFrom:
    secretKeyRef:
      {{- if $w.cipherSecret }}
      name: {{ $w.cipherSecret | quote }}
      key: pass
      {{- else }}
      name: {{ $full }}-database-passwords
      key: archive
      {{- end }}
{{- if and (eq $w.store "s3") $w.existingSecret }}
- name: PGBACKREST_REPO1_S3_KEY
  valueFrom:
    secretKeyRef:
      name: {{ $w.existingSecret | quote }}
      key: key
- name: PGBACKREST_REPO1_S3_KEY_SECRET
  valueFrom:
    secretKeyRef:
      name: {{ $w.existingSecret | quote }}
      key: secret
{{- else if eq $w.store "azure" }}
- name: PGBACKREST_REPO1_AZURE_ACCOUNT
  valueFrom:
    secretKeyRef:
      name: {{ $w.existingSecret | quote }}
      key: account
- name: PGBACKREST_REPO1_AZURE_KEY
  valueFrom:
    secretKeyRef:
      name: {{ $w.existingSecret | quote }}
      key: key
{{- end }}
{{- end -}}

{{/*
Where the archive is reached from a pod: its configuration for the role,
the volume beside the sealed backups where it is kept there, and a gcs
service account's key. Mounts under a container's volumeMounts, volumes
under the pod's. Each takes a dict: top, role.
*/}}
{{- define "meridian-runtime.walArchiveMounts" -}}
{{- $w := .top.Values.backup.walArchive -}}
- name: archive-config
  mountPath: /etc/pgbackrest
  readOnly: true
{{- if eq $w.store "volume" }}
- name: archive-store
  mountPath: /backups
{{- else if and (eq $w.store "gcs") $w.existingSecret }}
- name: archive-store
  mountPath: /etc/pgbackrest-store
  readOnly: true
{{- end }}
{{- end -}}

{{- define "meridian-runtime.walArchiveVolumes" -}}
{{- $w := .top.Values.backup.walArchive -}}
{{- $full := include "meridian-runtime.fullname" .top -}}
- name: archive-config
  configMap:
    name: {{ $full }}-pgbackrest
    items:
      - key: {{ .role }}.conf
        path: pgbackrest.conf
{{- if eq $w.store "volume" }}
- name: archive-store
  persistentVolumeClaim:
    claimName: {{ .top.Values.backup.existingClaim | default (printf "%s-backup" $full) }}
{{- else if and (eq $w.store "gcs") $w.existingSecret }}
- name: archive-store
  secret:
    secretName: {{ $w.existingSecret | quote }}
    items:
      - key: key
        path: key
{{- end }}
{{- end -}}

{{/*
Internal TLS (values.yaml's tls.internal; ruling 1084; templates/internal-
tls.yaml). A client's three pieces -- the CA's certificate at
MERIDIAN_INTERNAL_TLS_CA, its mount, its volume -- each nothing with it off,
so every component that reaches the database or the broker includes all
three beside its others. A server's certificate and key are its own pods'
alone (internalTlsServerVolume, by server: database, broker, sidecars).
*/}}
{{- define "meridian-runtime.internalTlsEnv" -}}
{{- if .Values.tls.internal.enabled }}
- name: MERIDIAN_INTERNAL_TLS_CA
  value: /etc/meridian-internal-tls/ca.crt
{{- end }}
{{- end -}}

{{- define "meridian-runtime.internalTlsMount" -}}
{{- if .Values.tls.internal.enabled }}
- name: internal-tls-ca
  mountPath: /etc/meridian-internal-tls
  readOnly: true
{{- end }}
{{- end -}}

{{- define "meridian-runtime.internalTlsVolume" -}}
{{- if .Values.tls.internal.enabled }}
- name: internal-tls-ca
  secret:
    secretName: {{ include "meridian-runtime.fullname" . }}-internal-tls-ca
    items:
      - key: ca.crt
        path: ca.crt
{{- end }}
{{- end -}}

{{/* A server's own certificate, key and the CA: a dict, top and server. */}}
{{- define "meridian-runtime.internalTlsServerVolume" -}}
{{- if .top.Values.tls.internal.enabled }}
- name: internal-tls-server
  secret:
    secretName: {{ include "meridian-runtime.fullname" .top }}-internal-tls-{{ .server }}
    # Readable by the pod's group, as the dashboard's signing key is: a
    # server that runs as its own user is given the group (fsGroup).
    defaultMode: 0440
{{- end }}
{{- end -}}

{{/* libpq's own variables, for a container whose tools are psql and
pg_dump: verified against the internal CA by the name connected to. */}}
{{- define "meridian-runtime.internalTlsLibpqEnv" -}}
{{- if .Values.tls.internal.enabled }}
- name: PGSSLMODE
  value: verify-full
- name: PGSSLROOTCERT
  value: /etc/meridian-internal-tls/ca.crt
{{- end }}
{{- end -}}
