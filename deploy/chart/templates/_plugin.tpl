{{/*
  A plugin and its sidecar, as one Deployment: the one shape every plugin
  runs in, whether the chart runs it from sidecars[] or the launcher launched
  it (decisions/019: one template). sidecar.yaml renders the first; the
  launcher fills in the second from launcher.yaml's copy, where the instance,
  image, roles and tags are placeholders.

  Takes a dict: top (the chart), name (the Deployment's), instance, roles and
  tags (comma-joined), brokerSecret and brokerKey (where the sidecar's broker
  credential is), plugin (image, pullPolicy, args, env, existingSecret,
  resources, imagePullSecret; absent for a sidecar alone), imageTag,
  launched, which marks what the launcher made and alone may remove, and
  live: the live shape of a development deployment
  (spec/live-plugin-development), where the plugin runs the files sent to it
  since its image, from a folder it shares with its sidecar.

  A plugin's sidecar, one Deployment each. The plugin runs as a second
  container in this pod, because the sidecar binds loopback: a sidecar
  reachable across the network is a way around the boundary it enforces.

  One per plugin rather than one shared. A `Sidecar` holds one registration,
  and a second plugin admitted on a shared one would inherit the first's
  grants, which was verified rather than assumed.
*/}}
{{- define "meridian-runtime.plugin" -}}
{{- $top := .top -}}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ .name }}
  labels:
    meridian.dev/component: sidecar
    meridian.dev/instance: {{ .instance | quote }}
    {{- if .launched }}
    meridian.dev/launched: "true"
    {{- end }}
    {{- if .live }}
    meridian.dev/live: "true"
    {{- end }}
    {{- include "meridian-runtime.labels" $top | nindent 4 }}
  {{- if .launched }}
  annotations:
    # The roles it was approved with (W8.3), which the broker\'s process reads
    # to admit its credential with their topics (the registry spec, decision 3).
    meridian.dev/roles: {{ .roles | quote }}
  {{- end }}
spec:
  replicas: 1
  revisionHistoryLimit: {{ $top.Values.revisionHistoryLimit }}
  # One copy of an instance at a time, always: a rolling update would start the
  # new pod beside the old, two plugins under one instance's name, hostname
  # and credential. The old stops first, and the page is away for the moment
  # between.
  strategy:
    type: Recreate
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
        no Service made per plugin (the registry spec, decision 4).
      */}}
      hostname: {{ .instance }}
      subdomain: {{ include "meridian-runtime.fullname" $top }}-sidecars
      {{- /*
        Live: the plugin (65532) and its sidecar share the live folder, each
        replacing what the other wrote, so the pod shares a group they both
        write it as.
      */}}
      {{- $podSecurity := $top.Values.podSecurityContext | default dict }}
      {{- if .live }}
      {{- $podSecurity = merge (dict "fsGroup" 65532) $podSecurity }}
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
            - name: MERIDIAN_PLUGIN_TAGS
              value: {{ .tags | quote }}
            - name: RUST_LOG
              value: {{ $top.Values.logLevel | quote }}
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
            {{- end }}
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
          {{- end }}
          {{- if or $top.Values.dashboard.enabled .live }}
          volumeMounts:
            {{- if $top.Values.dashboard.enabled }}
            - name: dashboard-keys
              mountPath: /etc/meridian/dashboard-keys
              readOnly: true
            {{- end }}
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
          No broker credential -- that seam is the boundary. And not its roles
          or tags: a plugin that named its own roles would be
          choosing its own privileges (decision 007), so it learns them from
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
            {{- if $.live }}
            - name: MERIDIAN_LIVE_DIR
              value: /plugin/live
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
          resources:
            {{- toYaml (.resources | default $top.Values.resources) | nindent 12 }}
        {{- end }}
      {{- if or .plugin $top.Values.dashboard.enabled }}
      volumes:
        {{- if .plugin }}
        - name: plugin-tmp
          emptyDir: {}
        {{- end }}
        {{- if .live }}
        - name: live
          emptyDir:
            sizeLimit: 256Mi
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
