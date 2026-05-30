{{- define "pharos.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "pharos.fullname" -}}
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

{{- define "pharos.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
app.kubernetes.io/name: {{ include "pharos.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "pharos.selectorLabels" -}}
app.kubernetes.io/name: {{ include "pharos.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "pharos.ui.selectorLabels" -}}
app.kubernetes.io/name: {{ include "pharos.name" . }}-ui
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* true when the configured database is SQLite (db PVC needed) */}}
{{- define "pharos.usesSqlite" -}}
{{- if hasPrefix "sqlite:" .Values.config.database.url -}}true{{- end -}}
{{- end -}}

{{/* Render /etc/pharos/config.toml from values; omit null/empty optionals. */}}
{{- define "pharos.configToml" -}}
{{- $s := .Values.config.server -}}
[server]
bind = {{ $s.bind | quote }}
name = {{ $s.name | quote }}
{{- if $s.uiDir }}
ui_dir = {{ $s.uiDir | quote }}
{{- end }}
{{- if $s.imageCacheDir }}
image_cache_dir = {{ $s.imageCacheDir | quote }}
{{- end }}
image_seek_seconds = {{ $s.imageSeekSeconds | int64 }}
{{- if $s.transcodeCacheDir }}
transcode_cache_dir = {{ $s.transcodeCacheDir | quote }}
{{- end }}
transcode_cache_max_bytes = {{ $s.transcodeCacheMaxBytes | int64 }}
{{- if $s.trickplayCacheDir }}
trickplay_cache_dir = {{ $s.trickplayCacheDir | quote }}
{{- end }}
trickplay_cache_max_bytes = {{ $s.trickplayCacheMaxBytes | int64 }}
trickplay_interval_ms = {{ $s.trickplayIntervalMs | int64 }}
trickplay_widths = [{{ range $i, $w := $s.trickplayWidths }}{{ if $i }}, {{ end }}{{ $w | int64 }}{{ end }}]
hwaccel = {{ $s.hwaccel | quote }}
transcode_hw_session_cap = {{ $s.transcodeHwSessionCap | int64 }}
transcode_probe_caps = {{ $s.transcodeProbeCaps }}
subtitle_cache_max_bytes = {{ $s.subtitleCacheMaxBytes | int64 }}
subtitle_cache_max_entries = {{ $s.subtitleCacheMaxEntries | int64 }}
{{- if $s.liveTvM3u }}
live_tv_m3u = {{ $s.liveTvM3u | quote }}
{{- end }}
{{- if $s.liveTvXmltv }}
live_tv_xmltv = {{ $s.liveTvXmltv | quote }}
{{- end }}
ssdp_enabled = {{ $s.ssdpEnabled }}
{{- if $s.ssdpAdvertiseUrl }}
ssdp_advertise_url = {{ $s.ssdpAdvertiseUrl | quote }}
{{- end }}
played_threshold_pct = {{ $s.playedThresholdPct | int64 }}
scan_rate_limit_ms = {{ $s.scanRateLimitMs | int64 }}
library_watch_enabled = {{ $s.libraryWatchEnabled }}
library_poll_interval_secs = {{ $s.libraryPollIntervalSecs | int64 }}

[obs]
log_level = {{ .Values.config.obs.logLevel | quote }}
{{- if .Values.config.obs.otlpEndpoint }}
otlp_endpoint = {{ .Values.config.obs.otlpEndpoint | quote }}
{{- end }}
{{- if .Values.config.obs.logDir }}
log_dir = {{ .Values.config.obs.logDir | quote }}
{{- end }}

[media]
roots = [{{ range $i, $r := .Values.config.media.roots }}{{ if $i }}, {{ end }}{{ $r | quote }}{{ end }}]
{{- range .Values.config.media.libraries }}

[[media.libraries]]
path = {{ .path | quote }}
{{- if .name }}
name = {{ .name | quote }}
{{- end }}
{{- if .kind }}
kind = {{ .kind | quote }}
{{- end }}
{{- end }}

[database]
url = {{ .Values.config.database.url | quote }}
{{- end -}}
