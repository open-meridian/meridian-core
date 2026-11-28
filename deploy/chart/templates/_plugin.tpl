{{/*
  A plugin and its sidecar, as one Deployment: the one shape every plugin
  runs in, whether the chart runs it from sidecars[] or the launcher launched
  it (decisions/019: one template). sidecar.yaml renders the first; the
  launcher fills in the second from launcher.yaml's copy, where the instance,
  image and roles are placeholders.

  Takes a dict: top (the chart), name (the Deployment's), instance, roles
  (comma-joined), brokerSecret and brokerKey (where the sidecar's broker
  credential is), plugin (image, pullPolicy, args, env, existingSecret,
  resources, imagePullSecret; absent for a sidecar alone), imageTag, limits
  (the plugin's own share at its sidecar, overriding sidecar.limits),
  launched, which marks what the launcher made and alone may remove, and
  live: the live shape of a development deployment
  (spec/live-plugin-development), where the plugin runs the files sent to it
  since its image, from a folder it shares with its sidecar; and storage: an
  edge plugin's shape (decisions/028), which mounts its instance's claim,
  `<fullname>-storage-<instance>`, in the plugin's container alone, at the
  path MERIDIAN_STORAGE_DIR names; and shards and standby (step 3d; the
  resilience spec's section 7): the instance as several pods of this one
  Deployment, one per shard and one more for a standby, each sidecar told
  the shard count and given its instance's own account,
  `<fullname>-shards-<instance>` -- in its own container alone, never the
  plugin's -- to take a Lease `<instance>-shard-<n>`, made for it (item
  31). Unset, or one shard and no standby, the instance's one pod as
  ever, with no lease.

  A plugin's sidecar, one Deployment each. The plugin runs as a second
  container in this pod, because the sidecar binds loopback: a sidecar
  reachable across the network is a way around the boundary it enforces.

  One per plugin rather than one shared. A `Sidecar` holds one registration,
  and a second plugin admitted on a shared one would inherit the first's
  grants, which was verified rather than assumed.
*/}}
{{- define "meridian-runtime.plugin" -}}
{{- $top := .top -}}
{{- $shards := max 1 (int (.shards | default 1)) -}}
{{- $several := or (gt $shards 1) .standby -}}
{{- if gt $shards 64 -}}
{{- fail (printf "%s is given %d shards; at most 64 (PluginDeclaration.most_shards)" .instance $shards) -}}
{{- end -}}
{{- /* A launched instance of several pods keeping storage shares one claim,
       its shared storage, which the launcher makes, copying the
       instance's own claim into it first where one is kept. A sidecars[]
       instance has no launcher to copy its own claim, so its storage stays
       one pod's. */ -}}
{{- if and $several .storage (not .launched) -}}
{{- fail (printf "%s is given shards or a standby pod and its own storage: a sidecars[] instance's storage is one pod's; launch it from the catalogue to run it as several pods on shared storage (pluginStorage.shared.storageClassName)" .instance) -}}
{{- end -}}
{{- if and $several .live -}}
{{- fail (printf "%s is given shards or a standby pod and the live shape: a live instance is one pod" .instance) -}}
{{- end -}}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ .name }}
  labels:
    meridian.dev/component: sidecar
    meridian.dev/instance: {{ .instance | quote }}
    {{- if .launched }}
    meridian.dev/launched: "true"
    # INTERIM(v33): its sidecar asks on inboxes named for its credential,
    # which preflight tells from one launched before them (review-core-2c,
    # finding 1); gone once no deployment predates them.
    meridian.dev/sidecar-inboxes: per-credential
    {{- end }}
    {{- if .live }}
    meridian.dev/live: "true"
    {{- end }}
    {{- include "meridian-runtime.labels" $top | nindent 4 }}
  {{- if .launched }}
  annotations:
    # The roles it was approved with (W8.3), which the broker\'s process reads
    # to admit its credential with their topics (the registry spec).
    meridian.dev/roles: {{ .roles | quote }}
  {{- end }}
spec:
  replicas: {{ if $several }}{{ add $shards (ternary 1 0 (.standby | default false)) }}{{ else }}1{{ end }}
  revisionHistoryLimit: {{ $top.Values.revisionHistoryLimit }}
  {{- if $several }}
  # Several pods of the instance (step 3d), each fenced by its shard's
  # Lease: a new pod starts beside the old as a standby, an old pod gives
  # its Lease up as it stops, and the new one takes it -- a shard at a time,
  # the rest serving throughout. Recreate would stop every shard and the
  # standby together. A standby pod is never ready, by design, so it is one
  # more that may be unavailable: with none allowed the rollout could never
  # stop an old pod.
  strategy:
    type: RollingUpdate
    rollingUpdate:
      maxSurge: 1
      maxUnavailable: {{ add 1 (ternary 1 0 (.standby | default false)) }}
  {{- else }}
  # One copy of an instance at a time, always: a rolling update would start the
  # new pod beside the old, two plugins under one instance's name, hostname
  # and credential. The old stops first, and the page is away for the moment
  # between.
  strategy:
    type: Recreate
  {{- end }}
  selector:
    matchLabels:
      app.kubernetes.io/name: {{ include "meridian-runtime.name" $top }}
      app.kubernetes.io/instance: {{ $top.Release.Name }}
      meridian.dev/instance: {{ .instance | quote }}
  template:
    metadata:
      labels:
        {{- include "meridian-runtime.labels" $top | nindent 8 }}
        meridian.dev/component: sidecar
        meridian.dev/instance: {{ .instance | quote }}
        {{- if .launched }}
        meridian.dev/launched: "true"
        {{- end }}
        {{- if .live }}
        meridian.dev/live: "true"
        {{- end }}
    spec:
      {{- /*
        Named for its instance under the one Service every plugin shares
        (sidecars.yaml), so `{instance}.{release}-sidecars` resolves to this
        pod and nothing else: how the dashboard reaches its front door, with
        no Service made per plugin (the registry spec). An
        instance of several pods (step 3d) resolves to each of its shards
        that is ready -- never its standby, whose front door is closed --
        and a page request reaches any one of them: the instance's page is
        its settings and its own state, the same at each; what a shard
        keeps of the instance's subjects is its sidecar's, not its page's.
      */}}
      hostname: {{ .instance }}
      subdomain: {{ include "meridian-runtime.fullname" $top }}-sidecars
      {{- if $several }}
      {{- /*
        Several pods of the instance (step 3d): each sidecar takes a shard's
        Lease under its instance's own account, whose Role names that
        instance's shards' Leases alone: made by the
        launcher for a launched instance, by this chart for a sidecars[]
        one (meridian-runtime.shardsAccount). Its token is mounted in the
        sidecar's container alone, below; the plugin's holds none.
      */}}
      serviceAccountName: {{ include "meridian-runtime.fullname" $top }}-shards-{{ .instance }}
      automountServiceAccountToken: false
      {{- end }}
      {{- /*
        Live: the plugin (65532) and its sidecar share the live folder, each
        replacing what the other wrote, so the pod shares a group they both
        write it as.
      */}}
      {{- $podSecurity := $top.Values.podSecurityContext | default dict }}
      {{- if .live }}
      {{- $podSecurity = merge (dict "fsGroup" 65532) $podSecurity }}
      {{- end }}
      {{- /*
        Storage: the plugin writes its claim as the group a fresh volume is
        given, unless podSecurityContext says otherwise (a cluster that
        assigns its own, as OpenShift does, sets pluginStorage.fsGroup empty).
      */}}
      {{- if and .storage $top.Values.pluginStorage.fsGroup }}
      {{- $podSecurity = merge (deepCopy $podSecurity) (dict "fsGroup" $top.Values.pluginStorage.fsGroup "fsGroupChangePolicy" "OnRootMismatch") }}
      {{- end }}
      {{- with $podSecurity }}
      securityContext:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .plugin }}{{- with .imagePullSecret }}
      imagePullSecrets:
        - name: {{ . }}
      {{- end }}{{- end }}
      {{- if and .live .plugin }}
      {{- /*
        Live: the plugin's files copied from its image into the shared live
        folder before anything starts, group-writable, so its sidecar -- which
        holds no capability, root or not -- can replace them with the files
        it is sent. The dev runner finds the folder seeded and runs it.
      */}}
      initContainers:
        - name: seed
          image: {{ .plugin.image | quote }}
          imagePullPolicy: {{ .plugin.pullPolicy | default "IfNotPresent" }}
          command:
            - sh
            - -c
            - |
              set -e
              cd /plugin
              for part in * .[!.]*; do
                [ -e "$part" ] || continue
                [ "$part" = live ] && continue
                cp -R "$part" live/
              done
              # What was copied, not the folder: that is the volume's, and
              # not this user's to change.
              find live -mindepth 1 -exec chmod g+rwX {} +
              find live -mindepth 1 -type d -exec chmod g+s {} +
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities:
              drop: [ALL]
          volumeMounts:
            - name: live
              mountPath: /plugin/live
      {{- end }}
      containers:
        - name: sidecar
          image: "{{ $top.Values.image.repository }}:{{ .imageTag | default $top.Values.image.tag }}"
          imagePullPolicy: {{ $top.Values.image.pullPolicy }}
          args: ["meridian-sidecar"]
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities:
              drop: [ALL]
          env:
            - name: MERIDIAN_DEPLOYMENT_ID
              value: {{ $top.Values.deployment.id | quote }}
            - name: MERIDIAN_SIDECAR_ADDRESS
              value: {{ $top.Values.sidecar.address | quote }}
            - name: MERIDIAN_INSTANCE_ID
              value: sidecar-{{ .instance }}
            {{/*
              Assigned here, at launch, never declared by the plugin: a plugin
              that named its own roles would be choosing its own privileges.
              What they may do is the contract's, compiled into the image
              (decisions/020); none, a plugin admitted with no topics.
            */}}
            - name: MERIDIAN_PLUGIN_INSTANCE_ID
              value: {{ .instance | quote }}
            - name: MERIDIAN_PLUGIN_ROLES
              value: {{ .roles | quote }}
            - name: RUST_LOG
              value: {{ $top.Values.logLevel | quote }}
            {{- if $several }}
            {{- /* Its instance's shard count, this pod's name and the
                   leases' timing (step 3d): it serves
                   its plugin once a shard's Lease is its own. */}}
            {{- include "meridian-runtime.leaseTiming" $top }}
            - name: MERIDIAN_PLUGIN_SHARDS
              value: {{ $shards | quote }}
            - name: MERIDIAN_POD_NAME
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name
            - name: MERIDIAN_LEASE_RENEW_SECONDS
              value: {{ $top.Values.leases.renewSeconds | quote }}
            - name: MERIDIAN_LEASE_DURATION_SECONDS
              value: {{ $top.Values.leases.durationSeconds | quote }}
            {{- end }}
            {{- /* In `ha`, a data plugin's lake writes held by the lake's
                   write-ahead stream before they are recorded (contract v30,
                   stage E); the sidecar of any other role ignores it. */}}
            {{- with (include "meridian-runtime.writeAheadEnv" $top) }}{{ . | nindent 12 }}{{- end }}
            {{- /* Its own budget, and the plugin's share at it (stage M; the
                   resilience spec's section 10a): past the share only this
                   plugin is refused. A sidecars[] entry's own `limits`
                   overrides the chart's sidecar.limits. The plugin's
                   answers in flight (answerMiB) and its read_after session
                   (256 KiB, a MiB here) are held apart from the sidecar's
                   own budget, so a plugin flooding it cannot starve the
                   sidecar's own work; a share that leaves it nothing is
                   refused here. */}}
            {{- $answerMiB := (.limits | default dict).answerMiB | default ($top.Values.sidecar.limits | default dict).answerMiB | default 32 }}
            {{- include "meridian-runtime.memoryEnv" (dict "top" $top "container" "sidecar" "values" ($top.Values.sidecar | default dict) "resources" $top.Values.resources "heldMiB" (add 1 (int64 $answerMiB)) "name" "sidecar") | nindent 12 }}
            {{- include "meridian-runtime.sidecarLimitsEnv" (dict "top" $top "limits" .limits) | nindent 12 }}
            {{- if $top.Values.dashboard.enabled }}
            {{/*
              The front door: the dashboard's requests for the plugin's page,
              each carrying an assertion signed by a key this reads from the
              mount below, by key id, when it first meets one.
            */}}
            - name: MERIDIAN_FRONT_DOOR_ADDRESS
              value: "0.0.0.0:{{ $top.Values.sidecar.frontDoorPort }}"
            - name: MERIDIAN_DASHBOARD_KEYS_DIR
              value: /etc/meridian/dashboard-keys
            {{- if $top.Values.tls.internal.enabled }}
            {{- /* Served over TLS alone, under the sidecars'
                   names, the dashboard verifying it against the CA. */}}
            - name: MERIDIAN_INTERNAL_TLS_CERT
              value: /etc/meridian-internal-tls-server/tls.crt
            - name: MERIDIAN_INTERNAL_TLS_KEY
              value: /etc/meridian-internal-tls-server/tls.key
            {{- end }}
            {{- end }}
            {{- with (include "meridian-runtime.internalTlsEnv" $top) }}{{ . | nindent 12 }}{{- end }}
            {{/*
              The credential this instance presents, which the plugin's
              container has no mount for. Containers in a pod share a network
              namespace and not a filesystem, and that seam is the boundary.
            */}}
            - name: MERIDIAN_BROKER_URL
              valueFrom:
                secretKeyRef:
                  name: {{ .brokerSecret }}
                  key: {{ .brokerKey | quote }}
            {{- if .live }}
            {{/*
              The live folder, and that this is a development deployment:
              both, or the sidecar answers no development request.
            */}}
            - name: MERIDIAN_LIVE_DIR
              value: /plugin/live
            - name: MERIDIAN_DEVELOPMENT
              value: "true"
            {{- end }}
          {{- if $top.Values.dashboard.enabled }}
          ports:
            - name: front-door
              containerPort: {{ $top.Values.sidecar.frontDoorPort }}
          {{- if $several }}
          {{- /* A standby's front door is closed until it holds a shard:
                 not ready, so the sidecars' Service never names it. */}}
          readinessProbe:
            tcpSocket:
              port: front-door
            periodSeconds: 2
          {{- end }}
          {{- end }}
          {{- if or $top.Values.dashboard.enabled .live $top.Values.tls.internal.enabled $several }}
          volumeMounts:
            {{- if $several }}
            - name: member-token
              mountPath: /var/run/secrets/kubernetes.io/serviceaccount
              readOnly: true
            {{- end }}
            {{- if $top.Values.dashboard.enabled }}
            - name: dashboard-keys
              mountPath: /etc/meridian/dashboard-keys
              readOnly: true
            {{- if $top.Values.tls.internal.enabled }}
            - name: internal-tls-server
              mountPath: /etc/meridian-internal-tls-server
              readOnly: true
            {{- end }}
            {{- end }}
            {{- with (include "meridian-runtime.internalTlsMount" $top) }}{{ . | nindent 12 }}{{- end }}
            {{- if .live }}
            - name: live
              mountPath: /plugin/live
            {{- end }}
          {{- end }}
          resources:
            {{- toYaml $top.Values.resources | nindent 12 }}
        {{- with .plugin }}
        {{- /*
          The plugin. It joins this pod rather than running as a workload of
          its own, because the sidecar binds loopback: sharing the network
          namespace is what lets the plugin dial it, and nothing outside the
          pod can.

          It is given the sidecar's address and nothing else of the sidecar's.
          No broker credential -- that seam is the boundary. And not its
          roles: a plugin that named its own roles would be
          choosing its own privileges, so it learns them from
          the sidecar's reply to registration instead.

          A sidecar with no plugin still renders, alone, as it did before this
          existed.
        */}}
        - name: plugin
          image: {{ required "a plugin needs its image" .image | quote }}
          imagePullPolicy: {{ .pullPolicy | default "IfNotPresent" }}
          {{- if $.live }}
          {{- /*
            Live: the SDK's dev runner, which runs the plugin's own entry
            point from the live folder and restarts it on each change.
          */}}
          command: ["meridian-dev", "run"]
          {{- else }}
          {{- with .args }}
          args:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          {{- end }}
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities:
              drop: [ALL]
          env:
            - name: MERIDIAN_SIDECAR_ADDRESS
              value: {{ $top.Values.sidecar.address | quote }}
            {{- /*
              Where it runs and as which account, from the downward API:
              what the SDK names when the archive's bucket refuses
              the account.
            */}}
            - name: MERIDIAN_POD_NAMESPACE
              valueFrom:
                fieldRef:
                  fieldPath: metadata.namespace
            - name: MERIDIAN_POD_SERVICE_ACCOUNT
              valueFrom:
                fieldRef:
                  fieldPath: spec.serviceAccountName
            {{- if $.live }}
            - name: MERIDIAN_LIVE_DIR
              value: /plugin/live
            {{- end }}
            {{- if $several }}
            {{- /* Its instance's shard count, the value and meaning its
                   sidecar is given; unset, an
                   instance of one pod. Which shard it serves it learns at
                   registration (RegisterReply.shard). */}}
            - name: MERIDIAN_PLUGIN_SHARDS
              value: {{ $shards | quote }}
            {{- end }}
            {{- if $.storage }}
            {{- /*
              Its instance's storage (decisions/028): durable, its own, and
              no other plugin's. What it keeps there is its raw external
              records and its working state; a fact anyone else needs still
              goes through the sidecar.
            */}}
            - name: MERIDIAN_STORAGE_DIR
              value: /var/lib/meridian/storage
            {{- end }}
            {{- range $name, $value := .env }}
            - name: {{ $name }}
              value: {{ $value | quote }}
            {{- end }}
          {{- with .existingSecret }}
          {{- /*
            The vendor's credentials, as environment variables from a Secret
            the administrator created. Never rendered here: the chart creates
            no secret, for the same reason it creates no key.
          */}}
          envFrom:
            - secretRef:
                name: {{ . }}
          {{- end }}
          volumeMounts:
            {{- /*
              The root filesystem is read-only, as the sidecar's is; a
              scratch directory is the one writable place, because most
              runtimes need one somewhere.
            */}}
            - name: plugin-tmp
              mountPath: /tmp
            {{- if $.live }}
            - name: live
              mountPath: /plugin/live
            {{- end }}
            {{- if $.storage }}
            - name: storage
              mountPath: /var/lib/meridian/storage
            {{- end }}
          resources:
            {{- toYaml (.resources | default $top.Values.resources) | nindent 12 }}
        {{- end }}
      {{- if or .plugin $top.Values.dashboard.enabled $top.Values.tls.internal.enabled $several }}
      volumes:
        {{- if $several }}
        - name: member-token
          projected:
            defaultMode: 0444
            sources:
              - serviceAccountToken:
                  path: token
                  expirationSeconds: 3607
              - configMap:
                  name: kube-root-ca.crt
                  items:
                    - key: ca.crt
                      path: ca.crt
              - downwardAPI:
                  items:
                    - path: namespace
                      fieldRef:
                        fieldPath: metadata.namespace
        {{- end }}
        {{- with (include "meridian-runtime.internalTlsVolume" $top) }}{{ . | nindent 8 }}{{- end }}
        {{- if $top.Values.dashboard.enabled }}
        {{- with (include "meridian-runtime.internalTlsServerVolume" (dict "top" $top "server" "sidecars")) }}{{ . | nindent 8 }}{{- end }}
        {{- end }}
        {{- if .plugin }}
        - name: plugin-tmp
          emptyDir: {}
        {{- end }}
        {{- if .live }}
        - name: live
          emptyDir:
            sizeLimit: 256Mi
        {{- end }}
        {{- if and .storage .plugin }}
        - name: storage
          persistentVolumeClaim:
            {{- if $several }}
            {{- /* Several pods of the instance share its storage, a folder
                   per shard. */}}
            claimName: {{ include "meridian-runtime.fullname" $top }}-shared-storage-{{ .instance }}
            {{- else }}
            claimName: {{ include "meridian-runtime.fullname" $top }}-storage-{{ .instance }}
            {{- end }}
        {{- end }}
        {{- if $top.Values.dashboard.enabled }}
        {{- /*
          The dashboard's public keys, one file per key id. A ConfigMap
          mounted whole, not by subPath, so a key the Job makes after this pod
          started arrives in it without a restart.
        */}}
        - name: dashboard-keys
          configMap:
            name: {{ include "meridian-runtime.fullname" $top }}-dashboard-keys
        {{- end }}
      {{- end }}
{{- end -}}

{{/*
  A sidecars[] instance of several pods' own account, as the launcher makes a
  launched one's: a ServiceAccount, and a
  Role and RoleBinding of its name, `<fullname>-shards-<instance>`, the Role
  granting `get` and `update` on exactly the instance's shards' Leases, by
  name -- RBAC names no prefix and cannot narrow `create` to a name -- so
  each Lease is made here too, free, and kept: never deleted, by an upgrade
  or an uninstall, so its leaseTransitions only grows. Takes a dict: top,
  instance, shards (the count).
*/}}
{{- define "meridian-runtime.shardsAccount" -}}
{{- $top := .top -}}
{{- $name := printf "%s-shards-%s" (include "meridian-runtime.fullname" $top) .instance -}}
{{- $count := int (max 1 (int (.shards | default 1))) -}}
apiVersion: v1
kind: ServiceAccount
metadata:
  name: {{ $name }}
  labels:
    meridian.dev/instance: {{ .instance | quote }}
    {{- include "meridian-runtime.labels" $top | nindent 4 }}
automountServiceAccountToken: false
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: {{ $name }}
  labels:
    meridian.dev/instance: {{ .instance | quote }}
    {{- include "meridian-runtime.labels" $top | nindent 4 }}
rules:
  - apiGroups: ["coordination.k8s.io"]
    resources: ["leases"]
    verbs: ["get", "update"]
    resourceNames:
      {{- range $shard := until $count }}
      - {{ printf "%s-shard-%d" $.instance $shard | quote }}
      {{- end }}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: {{ $name }}
  labels:
    meridian.dev/instance: {{ .instance | quote }}
    {{- include "meridian-runtime.labels" $top | nindent 4 }}
subjects:
  - kind: ServiceAccount
    name: {{ $name }}
    namespace: {{ $top.Release.Namespace }}
roleRef:
  kind: Role
  name: {{ $name }}
  apiGroup: rbac.authorization.k8s.io
{{- range $shard := until $count }}
---
apiVersion: coordination.k8s.io/v1
kind: Lease
metadata:
  name: {{ printf "%s-shard-%d" $.instance $shard }}
  labels:
    meridian.dev/instance: {{ $.instance | quote }}
    {{- include "meridian-runtime.labels" $top | nindent 4 }}
  annotations:
    helm.sh/resource-policy: keep
spec: {}
{{- end }}
{{- end -}}
