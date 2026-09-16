(function() {
    class MpvPlayerBase {
        constructor({ events, appHost, appSettings }) {
            this.events = events;
            this.appHost = appHost;
            this.appSettings = appSettings;
            this.type = 'mediaplayer';

            this._duration = undefined;
            this._currentTime = null;
            this._paused = false;
            this._volume = 100;
            this._playRate = 1;
            this._muted = false;
            this._hasConnection = false;
            this._seeking = false;

            this._currentSrc = null;
            this._currentPlayOptions = null;
            this._started = false;

            this.handlers = {
                onPlaying: null,
                onTimeUpdate: null,
                onSeeking: () => { this._seeking = true; },
                onEnded: () => { this.onEndedInternal(); },
                onPause: () => {
                    this._paused = true;
                    this.events.trigger(this, 'pause');
                },
                onDuration: (duration) => { this._duration = duration; },
                onError: (error) => {
                    console.error(`[Media] [${this.logTag}] media error:`, error);
                    this.events.trigger(this, 'error', [{ type: 'mediadecodeerror' }]);
                }
            };

            this.setVolume(this.getSavedVolume() * 100, false);
        }

        connectSignals() {
            if (this._hasConnection) return;
            this._hasConnection = true;
            const p = window.api.player;
            p.playing.connect(this.handlers.onPlaying);
            p.positionUpdate.connect(this.handlers.onTimeUpdate);
            p.seeking.connect(this.handlers.onSeeking);
            p.finished.connect(this.handlers.onEnded);
            p.updateDuration.connect(this.handlers.onDuration);
            p.error.connect(this.handlers.onError);
            p.paused.connect(this.handlers.onPause);
        }

        disconnectSignals() {
            if (!this._hasConnection) return;
            this._hasConnection = false;
            const p = window.api.player;
            p.playing.disconnect(this.handlers.onPlaying);
            p.positionUpdate.disconnect(this.handlers.onTimeUpdate);
            p.seeking.disconnect(this.handlers.onSeeking);
            p.finished.disconnect(this.handlers.onEnded);
            p.updateDuration.disconnect(this.handlers.onDuration);
            p.error.disconnect(this.handlers.onError);
            p.paused.disconnect(this.handlers.onPause);
        }

        onEndedInternal() {
            this.events.trigger(this, 'stopped', [{ src: this._currentSrc }]);
            this._currentTime = null;
            this._currentSrc = null;
            this._currentPlayOptions = null;
        }

        currentSrc() { return this._currentSrc; }

        getDeviceProfile(item, options) {
            return this.appHost?.getDeviceProfile
                ? this.appHost.getDeviceProfile(item, options)
                : Promise.resolve({});
        }

        get mediaType() { return null; }

        _resolveTracks() {
            return {
                videoParam: MpvPlayerBase.TRACK_DISABLE,
                audioParam: 1,
                subParam: MpvPlayerBase.TRACK_DISABLE,
                externalAudioUrl: null,
                externalSubUrl: null
            };
        }

        _beforeLoad() {}

        setCurrentSrc(options) {
            return new Promise((resolve) => {
                const val = options.url;
                this._currentSrc = val;
                console.debug(`[Media] [${this.logTag}] Playing:`, val);

                const ms = Math.round((options.playerStartPositionTicks || 0) / 10000);
                this._currentPlayOptions = options;
                this._currentTime = ms;

                const { videoParam, audioParam, subParam, externalAudioUrl, externalSubUrl } = this._resolveTracks(options);
                this._beforeLoad(options);
                window.api.player.load(val,
                    {
                        startMilliseconds: ms,
                        autoplay: true,
                        isInfiniteStream: !!options.mediaSource?.IsInfiniteStream
                    },
                    { type: this.mediaType, metadata: options.item },
                    videoParam,
                    audioParam,
                    subParam,
                    externalAudioUrl,
                    externalSubUrl,
                    resolve);
            });
        }

        _emitPlaying() {
            if (this._paused) {
                this._paused = false;
                this.events.trigger(this, 'unpause');
            }
            this.events.trigger(this, 'playing');
            console.debug(`[Media] [${this.logTag}] playing event triggered`);
        }

        pause() { window.api.player.pause(); }
        resume() { this._paused = false; window.api.player.play(); }
        unpause() { window.api.player.play(); }
        paused() { return this._paused; }

        currentTime(val) {
            if (val != null) {
                this._currentTime = val;
                window.api.player.seekTo(val);
                return;
            }
            return this._currentTime;
        }

        currentTimeAsync() {
            return new Promise(resolve => window.api.player.getPosition(resolve));
        }

        duration() { return this._duration || null; }
        seekable() { return Boolean(this._duration); }
        getBufferedRanges() { return window._bufferedRanges || []; }

        setPlaybackRate(value) {
            this._playRate = value;
            window.api.player.setPlaybackRate(value * 1000);
        }

        getPlaybackRate() { return this._playRate || 1; }

        getSupportedPlaybackRates() {
            return [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 3.5, 4.0].map(id => ({ name: id + 'x', id }));
        }

        saveVolume(value) {
            if (value) this.appSettings.set('volume', value);
        }

        getSavedVolume() {
            return this.appSettings.get('volume') || 1;
        }

        setVolume(val, save = true) {
            val = Number(val);
            if (!isNaN(val)) {
                this._volume = val;
                if (save) {
                    this.appSettings.set('volume', (val || 100) / 100);
                    this.events.trigger(this, 'volumechange');
                }
                window.api.player.setVolume(val);
            }
        }

        getVolume() { return this._volume; }
        volumeUp() { this.setVolume(Math.min(this._volume + 2, 100)); }
        volumeDown() { this.setVolume(Math.max(this._volume - 2, 0)); }

        setMute(mute, triggerEvent = true) {
            this._muted = mute;
            window.api.player.setMuted(mute);
            if (triggerEvent) this.events.trigger(this, 'volumechange');
        }

        isMuted() { return this._muted; }
    }

    MpvPlayerBase.TRACK_DISABLE = 0;  

    window.MpvPlayerBase = MpvPlayerBase;
})();
