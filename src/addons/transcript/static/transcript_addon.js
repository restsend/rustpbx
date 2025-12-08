
(function () {
    // Wait for Alpine to be available
    document.addEventListener('alpine:init', () => {
        Alpine.data('transcriptAddon', (recordId) => ({
            recordId: recordId,
            transcript: {
                available: false,
                content: null,
                generated_at: null,
            },
            transcriptStatus: 'pending', // pending, processing, completed, failed
            transcriptLoading: false,
            transcriptVisible: false,
            asrProcessing: false,
            errorMessage: null,
            selectedLanguage: 'auto',
            languageOptions: [
                { value: 'auto', label: 'Auto Detect' },
                { value: 'zh', label: 'Chinese' },
                { value: 'en', label: 'English' },
                { value: 'ja', label: 'Japanese' },
                { value: 'ko', label: 'Korean' },
                { value: 'yue', label: 'Cantonese' },
            ],

            init() {
                // Try to get initial state from the main record object if available
                // We assume the main record object is available in the scope or we fetch it
                // For now, we'll fetch the transcript status
                this.checkTranscriptStatus();
            },

            async checkTranscriptStatus() {
                if (!this.recordId) return;

                try {
                    const response = await fetch(`/console/call-records/${this.recordId}/transcript`);
                    if (response.ok) {
                        const data = await response.json();
                        if (data && data.transcript) {
                            this.transcript = data.transcript;
                            this.transcriptStatus = 'completed';
                            this.transcriptVisible = true;
                        } else if (data && data.status) {
                            this.transcriptStatus = data.status;
                        }
                    }
                } catch (e) {
                    console.error("Failed to check transcript status", e);
                }
            },

            async requestTranscript(force = false) {
                if (this.asrProcessing || this.transcriptLoading) return;

                // If we already have it and not forcing, just toggle visibility
                if (this.transcriptStatus === 'completed' && !force) {
                    this.transcriptVisible = !this.transcriptVisible;
                    return;
                }

                this.asrProcessing = true;
                try {
                    const response = await fetch(`/console/call-records/${this.recordId}/transcript`, {
                        method: 'POST',
                        headers: {
                            'Content-Type': 'application/json',
                        },
                        body: JSON.stringify({
                            language: this.selectedLanguage,
                            action: 'transcribe',
                            force: force
                        })
                    });

                    if (response.ok) {
                        const data = await response.json();
                        this.transcriptStatus = 'processing';
                        this.errorMessage = null;
                        // Poll for status
                        this.pollStatus();
                    } else {
                        this.transcriptStatus = 'failed';
                        try {
                            const errorData = await response.json();
                            this.errorMessage = errorData.message || "Request failed";
                        } catch (e) {
                            this.errorMessage = "Failed to request transcript";
                        }
                        console.error("Failed to request transcript");
                    }
                } catch (e) {
                    console.error("Error requesting transcript", e);
                    this.transcriptStatus = 'failed';
                    this.errorMessage = e.message || "Network error";
                } finally {
                    this.asrProcessing = false;
                }
            },

            pollStatus() {
                const interval = setInterval(async () => {
                    if (this.transcriptStatus === 'completed' || this.transcriptStatus === 'failed') {
                        clearInterval(interval);
                        return;
                    }
                    await this.checkTranscriptStatus();
                }, 2000);
            },

            // Helpers for UI
            transcriptStatusTone(status) {
                switch ((status || '').toLowerCase()) {
                    case 'completed': return 'text-emerald-600';
                    case 'processing': return 'text-amber-500';
                    case 'failed': return 'text-rose-500';
                    default: return 'text-slate-400';
                }
            },

            transcriptStatusLabel(status) {
                switch ((status || '').toLowerCase()) {
                    case 'completed': return 'Ready';
                    case 'processing': return 'Processing';
                    case 'failed': return 'Failed';
                    default: return 'Pending';
                }
            },

            transcriptButtonLabel() {
                if (this.asrProcessing) return 'Submitting…';
                if (this.transcriptLoading) return 'Loading…';
                const status = (this.transcriptStatus || '').toLowerCase();
                if (status === 'processing') return 'Refresh status';
                if (status === 'failed') return 'Retry transcript';
                if (this.transcriptVisible) return 'Re-run transcript';
                if (this.transcriptStatus === 'completed') return 'Load transcript';
                return 'Request transcript';
            },

            formatDateTime(value) {
                if (!value) return '—';
                return new Date(value).toLocaleString();
            },

            // Timeline helpers
            transcriptTimeline() {
                const segments = Array.isArray(this.transcript?.segments) ? this.transcript.segments : [];
                if (!segments.length) {
                    return [];
                }
                const enriched = segments.map((segment, index) => {
                    const startValue = Number(segment?.start);
                    const endValue = Number(segment?.end);
                    const rawChannel = segment ? segment.channel : null;
                    let channelKey = 'mono';
                    if (rawChannel !== null && rawChannel !== undefined && rawChannel !== '') {
                        const numericChannel = Number(rawChannel);
                        channelKey = Number.isFinite(numericChannel) ? numericChannel : 'mono';
                    }
                    let side = 'mono';
                    if (channelKey !== 'mono') {
                        const numeric = Number(channelKey);
                        if (Number.isFinite(numeric)) {
                            if (numeric === 0) {
                                side = 'left';
                            } else if (numeric === 1) {
                                side = 'right';
                            } else {
                                side = numeric % 2 === 0 ? 'left' : 'right';
                            }
                        }
                    }
                    const label = segment?.speaker || this.channelLabel(channelKey);
                    const start = Number.isFinite(startValue) ? startValue : null;
                    const end = Number.isFinite(endValue) ? endValue : null;
                    return {
                        key: `timeline-${index}-${segment?.idx ?? index}-${start ?? ''}`,
                        segment,
                        side,
                        label,
                        start,
                        end,
                    };
                });
                enriched.sort((a, b) => {
                    if (a.start === null && b.start === null) {
                        return 0;
                    }
                    if (a.start === null) {
                        return 1;
                    }
                    if (b.start === null) {
                        return -1;
                    }
                    if (a.start === b.start) {
                        return 0;
                    }
                    return a.start - b.start;
                });
                return enriched;
            },
            transcriptAlignmentClass(side) {
                if (side === 'right') {
                    return 'justify-end';
                }
                if (side === 'mono') {
                    return 'justify-center';
                }
                return 'justify-start';
            },
            transcriptCardTone(side) {
                if (side === 'right') {
                    return 'border-emerald-200 bg-emerald-50/80';
                }
                if (side === 'left') {
                    return 'border-sky-200 bg-sky-50/80';
                }
                return 'border-slate-200 bg-white/90';
            },
            channelLabel(key) {
                if (key === 'mono' || key === null || key === undefined) {
                    return 'Mono channel';
                }
                const numeric = Number(key);
                if (!Number.isFinite(numeric)) {
                    return `Channel ${key}`;
                }
                if (numeric === 0) {
                    return 'Left channel';
                }
                if (numeric === 1) {
                    return 'Right channel';
                }
                return `Channel ${numeric + 1}`;
            },
            formatSegmentTimestamp(value) {
                if (value === undefined || value === null) {
                    return '';
                }
                const numeric = Number(value);
                if (!Number.isFinite(numeric)) {
                    return String(value);
                }
                return `${numeric.toFixed(1)}s`;
            },
            formatSegmentRange(start, end) {
                const startText = this.formatSegmentTimestamp(start);
                const endText = this.formatSegmentTimestamp(end);
                const hasStart = Boolean(startText);
                const hasEnd = end !== undefined && end !== null && end !== '' && Boolean(endText);
                if (hasStart && hasEnd) {
                    if (startText === endText) {
                        return startText;
                    }
                    return `${startText} → ${endText}`;
                }
                if (!hasStart && hasEnd) {
                    return endText;
                }
                return startText;
            }
        }));
    });

    // Inject HTML
    const injectTranscriptUI = () => {
        const overviewTab = document.getElementById('call-record-tab-overview');
        if (overviewTab && !document.getElementById('transcript-addon-ui')) {
            // Get record ID from the URL
            const parts = window.location.pathname.split('/');
            const recordId = parts[parts.length - 1];

            const container = document.createElement('div');
            container.id = 'transcript-addon-ui';
            container.className = "rounded-xl bg-white p-6 shadow-sm ring-1 ring-black/5 mt-4";
            container.setAttribute('x-data', `transcriptAddon('${recordId}')`);

            container.innerHTML = `
                <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
                    <div>
                        <h3 class="text-sm font-semibold text-slate-900">Transcript (Addon)</h3>
                        <p class="text-xs text-slate-500">Generate speech-to-text to accelerate QA and analytics.</p>
                        <div class="mt-2 flex items-center gap-2 text-xs font-semibold" :class="transcriptStatusTone(transcriptStatus)">
                            <span x-text="transcriptStatusLabel(transcriptStatus)"></span>
                            <span x-show="transcriptStatus === 'failed' && errorMessage" class="text-rose-600 font-normal" x-text="' - ' + errorMessage"></span>
                        </div>
                    </div>
                    <div class="flex w-full flex-col gap-2 sm:w-auto sm:flex-row sm:items-end sm:justify-end">
                        <div class="w-full sm:w-48">
                            <label class="mb-1 block text-[11px] font-semibold uppercase tracking-wide text-slate-400">Language</label>
                            <select class="w-full rounded-lg border border-slate-200 bg-white px-3 py-2 text-sm text-slate-700"
                                x-model="selectedLanguage" :disabled="asrProcessing">
                                <template x-for="option in languageOptions" :key="option.value">
                                    <option :value="option.value" x-text="option.label"></option>
                                </template>
                            </select>
                        </div>
                        <button type="button"
                            class="inline-flex items-center gap-2 rounded-lg bg-sky-600 px-3 py-2 text-sm font-semibold text-white transition hover:bg-sky-500 disabled:opacity-60"
                            :disabled="asrProcessing"
                            @click="requestTranscript(transcriptVisible)">
                            <span x-text="transcriptButtonLabel()"></span>
                        </button>
                    </div>
                </div>
                
                <div x-show="transcriptVisible" class="mt-4 border-t border-slate-100 pt-4">
                    <template x-if="transcript.segments && transcript.segments.length">
                        <div class="space-y-4">
                            <template x-for="item in transcriptTimeline()" :key="item.key">
                                <div class="flex w-full" :class="transcriptAlignmentClass(item.side)">
                                    <div class="max-w-[85%] rounded-lg border p-3 shadow-sm sm:max-w-[70%]"
                                        :class="transcriptCardTone(item.side)">
                                        <div class="mb-1 flex items-center justify-between gap-4 text-[11px] text-slate-500">
                                            <span class="font-semibold uppercase tracking-wide" x-text="item.label"></span>
                                            <span class="font-mono" x-text="formatSegmentRange(item.start, item.end)"></span>
                                        </div>
                                        <div class="whitespace-pre-wrap text-sm text-slate-700" x-text="item.segment.text"></div>
                                    </div>
                                </div>
                            </template>
                        </div>
                    </template>
                    <template x-if="!transcript.segments || !transcript.segments.length">
                        <div class="prose prose-sm max-w-none text-slate-600">
                            <pre x-text="transcript.content || transcript.text" class="whitespace-pre-wrap font-sans"></pre>
                        </div>
                    </template>
                </div>
            `;

            overviewTab.appendChild(container);
        }
    };

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', injectTranscriptUI);
    } else {
        injectTranscriptUI();
    }
})();
