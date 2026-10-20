(function() {
    class inputPlugin {
        constructor({ playbackManager, inputManager }) {
            this.name = 'Input Plugin';
            this.type = 'input';
            this.id = 'inputPlugin';
            this.playbackManager = playbackManager;
            this.inputManager = inputManager;
            this.positionInterval = null;
            this.artworkAbortController = null;
            this.pendingArtworkUrl = null;
            this.attachedPlayer = null;
            this.playerHandlers = null;
            this.onPlaybackStart = null;
            this.onPlaybackStop = null;
            this.lastQueueCaps = null;

            console.debug('[Media] inputPlugin constructed with playbackManager:', !!playbackManager);

            if (playbackManager && window.Events) {
                this.setupEvents(playbackManager);
            }
        }

        notifyMetadata(item) {
            if (!item || !window.jmpNative) return;
            const meta = {
                Name: item.Name || '',
                Type: item.Type || '',
                MediaType: item.MediaType || '',
                SeriesName: item.SeriesName || '',
                SeasonName: item.SeasonName || '',
                Album: item.Album || '',
                Artists: item.Artists || [],
                IndexNumber: item.IndexNumber || 0,
                RunTimeTicks: item.RunTimeTicks || 0,
                Id: item.Id || ''
            };
            console.debug('[Media] notifyMetadata:', meta.Name);
            window.jmpNative.notifyMetadata(JSON.stringify(meta));
            this.fetchAlbumArt(item);
        }

        getImageUrl(item, baseUrl) {
            const imageTags = item.ImageTags || {};
            const itemType = item.Type || '';
            const mediaType = item.MediaType || '';

            if (itemType === 'Episode') {
                if (item.SeriesId && item.SeriesPrimaryImageTag) {
                    return baseUrl + '/Items/' + item.SeriesId + '/Images/Primary?tag=' + item.SeriesPrimaryImageTag + '&maxWidth=512';
                }
                if (item.SeasonId && item.SeasonPrimaryImageTag) {
                    return baseUrl + '/Items/' + item.SeasonId + '/Images/Primary?tag=' + item.SeasonPrimaryImageTag + '&maxWidth=512';
                }
            }

            if (mediaType === 'Audio' || itemType === 'Audio') {
                if (item.AlbumId && item.AlbumPrimaryImageTag) {
                    return baseUrl + '/Items/' + item.AlbumId + '/Images/Primary?tag=' + item.AlbumPrimaryImageTag + '&maxWidth=512';
                }
            }

            if (imageTags.Primary && item.Id) {
                return baseUrl + '/Items/' + item.Id + '/Images/Primary?tag=' + imageTags.Primary + '&maxWidth=512';
            }
            if (item.BackdropImageTags && item.BackdropImageTags.length > 0 && item.Id) {
                return baseUrl + '/Items/' + item.Id + '/Images/Backdrop/0?tag=' + item.BackdropImageTags[0] + '&maxWidth=512';
            }

            return null;
        }

        fetchAlbumArt(item) {
            if (!item || !window.jmpNative) return;

            if (this.artworkAbortController) {
                this.artworkAbortController.abort();
                this.artworkAbortController = null;
            }

            let baseUrl = '';
            if (window.ApiClient && window.ApiClient.serverAddress) {
                baseUrl = window.ApiClient.serverAddress();
            }
            if (!baseUrl) return;

            const imageUrl = this.getImageUrl(item, baseUrl);
            if (!imageUrl) {
                console.debug('[Media] No album art URL found');
                return;
            }

            if (imageUrl === this.pendingArtworkUrl) {
                console.debug('[Media] Album art already pending for:', imageUrl);
                return;
            }

            this.pendingArtworkUrl = imageUrl;
            this.artworkAbortController = new AbortController();
            const signal = this.artworkAbortController.signal;

            console.debug('[Media] Fetching album art:', imageUrl);

            fetch(imageUrl, { signal })
                .then(response => {
                    if (!response.ok) throw new Error('Failed to fetch image');
                    return response.blob();
                })
                .then(blob => {
                    const reader = new FileReader();
                    reader.onloadend = () => {
                        if (signal.aborted) return;
                        const dataUri = reader.result;
                        console.debug('[Media] Album art fetched, sending data URI');
                        window.jmpNative.notifyArtwork(dataUri);
                        this.pendingArtworkUrl = null;
                    };
                    reader.readAsDataURL(blob);
                })
                .catch(err => {
                    if (err.name === 'AbortError') {
                        console.debug('[Media] Album art fetch aborted');
                    } else {
                        console.warn('[Media] Album art fetch failed:', err.message);
                    }
                    this.pendingArtworkUrl = null;
                });
        }

        // Every playbackManager query below names its player explicitly.
        // The manager's own defaults fall back to `_currentPlayer`, which
        // is null from onPlaybackStopped until the next item's
        // onPlaybackStarted — and that window is exactly when mpv's
        // load-paused `pause` signal and the first `playing` arrive,
        // because they are what resolves the play() promise that sets
        // it. `getCurrentTicks(null)` throws "player cannot be null", so
        // an implicit call from those handlers threw on every
        // stop-then-play. With no player at all there is nothing to
        // report; the native side already knows the position.
        _player(player) {
            if (player) return player;
            if (this.attachedPlayer) return this.attachedPlayer;
            const pm = this.playbackManager;
            if (!pm) return null;
            return (pm.getCurrentPlayer ? pm.getCurrentPlayer() : pm._currentPlayer) || null;
        }

        // Position in ms, or null when there is no player to ask.
        _currentTimeMs(player) {
            const pm = this.playbackManager;
            const p = this._player(player);
            if (!pm || !p || typeof pm.currentTime !== 'function') return null;
            const t = pm.currentTime(p);
            return (typeof t === 'number' && t >= 0) ? t : null;
        }

        _playerState(player) {
            const pm = this.playbackManager;
            const p = this._player(player);
            if (!pm || !p || typeof pm.getPlayerState !== 'function') return null;
            return pm.getPlayerState(p);
        }

        _playbackRate(player) {
            const p = this._player(player);
            return (p && typeof p.getPlaybackRate === 'function') ? p.getPlaybackRate() : 1.0;
        }

        startPositionUpdates() {
            const initialPos = this._currentTimeMs();
            if (initialPos !== null) {
                window.jmpNative.notifyPosition(Math.floor(initialPos));
            }

            this.positionTracking = {
                startTime: Date.now(),
                startPos: initialPos || 0,
                rate: this._playbackRate()
            };
        }

        resetPositionTracking() {
            this.positionTracking = {
                startTime: Date.now(),
                startPos: this._currentTimeMs() || 0,
                rate: this._playbackRate()
            };
        }

        checkPositionDrift() {
            if (!this.positionTracking || !this.playbackManager) return;
            const actual = this._currentTimeMs();
            if (actual === null) return;

            const elapsed = Date.now() - this.positionTracking.startTime;
            const expected = this.positionTracking.startPos + (elapsed * this.positionTracking.rate);
            const drift = actual - expected;

            if (Math.abs(drift) > 2000) {
                console.debug('[Media] Position drift detected: expected=' + Math.floor(expected) + ' actual=' + Math.floor(actual) + ' drift=' + Math.floor(drift));
                if (drift > 0) {
                    window.jmpNative.notifySeek(Math.floor(actual));
                } else {
                    window.jmpNative.notifyRateChange(0.0);
                    window.jmpNative.notifyPosition(Math.floor(actual));
                }
                this.resetPositionTracking();
            }
        }

        stopPositionUpdates() {
            this.positionTracking = null;
        }

        // Feeds the OS media session's next/previous capability. Two
        // boundaries look alike from here and are handled differently:
        // a real stop resets jellyfin-web's queue before `playbackstop`
        // fires, so an empty playlist is a true state (nothing to step
        // to) and is reported; the next item's `playing` arrives before
        // `setPlaylist`, so the same empty queue shows up again and is
        // deduplicated, and `playbackstart` right after carries the real
        // one. A non-empty playlist with no current index is the only
        // transient, and is skipped.
        updateQueueState() {
            try {
                if (!window.jmpNative) return;

                const pm = this.playbackManager;
                if (!pm) return;

                const qm = pm._playQueueManager;
                const playlist = qm?.getPlaylist();
                const currentIndex = qm?.getCurrentPlaylistIndex();

                let canNext = false;
                let canPrev = false;
                if (Array.isArray(playlist) && playlist.length > 0) {
                    if (typeof currentIndex !== 'number' || currentIndex < 0) {
                        console.debug('[Media] updateQueueState: playlist set but no current item yet (len=' + playlist.length + ')');
                        return;
                    }
                    canNext = currentIndex < playlist.length - 1;
                    const state = this._playerState();
                    const isMusic = state?.NowPlayingItem?.MediaType === 'Audio';
                    canPrev = isMusic ? true : (currentIndex > 0);
                }

                const last = this.lastQueueCaps;
                if (last && last.canNext === canNext && last.canPrev === canPrev) return;
                this.lastQueueCaps = { canNext, canPrev };

                console.debug('[Media] updateQueueState: idx=' + currentIndex + ' len=' + (playlist?.length || 0) + ' canNext=' + canNext + ' canPrev=' + canPrev);
                window.jmpNative.notifyQueueChange(canNext, canPrev);
            } catch (e) {
                console.error('[Media] updateQueueState error:', e);
            }
        }

        // The player object is a singleton, so this normally runs once per
        // page. Handler references are kept because jellyfin-web's
        // Events.off(obj, name) without the function removes nothing.
        attachPlayer(player) {
            this.detachPlayer();
            this.attachedPlayer = player;
            const self = this;

            this.playerHandlers = {
                playing: () => {
                    console.debug('[Media] player.playing event');
                    if (!window.jmpNative) return;
                    window.jmpNative.notifyPlaybackState('Playing');
                    self.updateQueueState();

                    const pos = self._currentTimeMs(player);
                    if (pos !== null) window.jmpNative.notifyPosition(Math.floor(pos));
                    self.resetPositionTracking();

                    window.jmpNative.notifyRateChange(self._playbackRate(player));
                },
                pause: () => {
                    console.debug('[Media] player.pause event');
                    if (!window.jmpNative) return;
                    window.jmpNative.notifyPlaybackState('Paused');
                    const pos = self._currentTimeMs(player);
                    if (pos !== null) window.jmpNative.notifyPosition(Math.floor(pos));
                },
                ratechange: () => {
                    const rate = self._playbackRate(player);
                    console.debug('[Media] player.ratechange event, rate:', rate);
                    if (window.jmpNative) {
                        window.jmpNative.notifyRateChange(rate);
                        const pos = self._currentTimeMs(player);
                        if (pos !== null) window.jmpNative.notifyPosition(Math.floor(pos));
                    }
                    self.resetPositionTracking();
                },
                timeupdate: () => {
                    self.checkPositionDrift();
                }
            };
            for (const [name, fn] of Object.entries(this.playerHandlers)) {
                window.Events.on(player, name, fn);
            }
        }

        detachPlayer() {
            const player = this.attachedPlayer;
            if (player && this.playerHandlers && window.Events) {
                for (const [name, fn] of Object.entries(this.playerHandlers)) {
                    window.Events.off(player, name, fn);
                }
            }
            this.playerHandlers = null;
            this.attachedPlayer = null;
        }

        setupEvents(pm) {
            console.debug('[Media] Setting up playbackManager events');
            const self = this;

            this.onPlaybackStart = (e, player) => {
                console.debug('[Media] playbackstart event, player:', !!player);

                const state = self._playerState(player);

                if (state && state.NowPlayingItem) {
                    self.notifyMetadata(state.NowPlayingItem);
                }

                console.debug('[Media] Sending Playing state from playbackstart');
                if (window.jmpNative) window.jmpNative.notifyPlaybackState('Playing');
                self.startPositionUpdates();
                self.updateQueueState();

                if (player && player !== self.attachedPlayer) {
                    self.attachPlayer(player);
                }
            };

            this.onPlaybackStop = (e, stopInfo) => {
                try {
                    console.debug('[Media] playbackstop event, stopInfo:', JSON.stringify(stopInfo));
                } catch (err) {
                    console.debug('[Media] playbackstop event, stopInfo: [unserializable]');
                }
                self.stopPositionUpdates();

                const isNavigating = !!(stopInfo && stopInfo.nextMediaType);
                if (!isNavigating) {
                    console.debug('[Media] Playback truly stopped, clearing state');
                    if (window.jmpNative) window.jmpNative.notifyPlaybackState('Stopped');
                } else {
                    console.debug('[Media] Navigating to next item, keeping metadata');
                }
                self.updateQueueState();
            };

            window.Events.on(pm, 'playbackstart', this.onPlaybackStart);
            window.Events.on(pm, 'playbackstop', this.onPlaybackStop);

            window.Events.on(pm, 'playlistitemremove', () => self.updateQueueState());
            window.Events.on(pm, 'playlistitemadd', () => self.updateQueueState());
            window.Events.on(pm, 'playlistitemchange', () => self.updateQueueState());

            const remap = {
                'play_pause': 'playpause',
                'play': 'play',
                'pause': 'pause',
                'stop': 'stop',
                'next': 'next',
                'previous': 'previous',
                'seek_forward': 'fastforward',
                'seek_backward': 'rewind'
            };

            window.api.input.hostInput.connect((actions) => {
                console.debug('[Media] hostInput received:', actions);
                actions.forEach(action => {
                    const mappedAction = remap[action] || action;
                    console.debug('[Media] Sending to inputManager:', mappedAction);
                    if (self.inputManager && typeof self.inputManager.handleCommand === 'function') {
                        self.inputManager.handleCommand(mappedAction, {});
                    } else {
                        console.warn('[Media] inputManager.handleCommand not available, inputManager:', !!self.inputManager);
                    }
                });
            });

            window.api.input.positionSeek.connect((positionMs) => {
                console.debug('[Media] positionSeek received:', positionMs);
                const currentPlayer = pm.getCurrentPlayer ? pm.getCurrentPlayer() : pm._currentPlayer;
                if (currentPlayer) {
                    const duration = pm.duration ? pm.duration(currentPlayer) : 0;
                    if (duration > 0) {
                        const percent = (positionMs * 10000) / duration * 100;
                        console.debug('[Media] Seeking to', percent.toFixed(2), '% (', positionMs, 'ms of', duration, 'ticks)');
                        pm.seekPercent(percent, currentPlayer);
                    }
                }
            });

            window.api.input.rateChanged.connect((rate) => {
                console.debug('[Media] rateChanged received:', rate);
                const currentPlayer = pm.getCurrentPlayer ? pm.getCurrentPlayer() : pm._currentPlayer;
                if (currentPlayer && typeof currentPlayer.setPlaybackRate === 'function') {
                    currentPlayer.setPlaybackRate(rate);
                }
            });

            console.debug('[Media] Events setup complete');
        }

        destroy() {
            this.stopPositionUpdates();
            if (this.artworkAbortController) {
                this.artworkAbortController.abort();
                this.artworkAbortController = null;
            }
            this.detachPlayer();
            if (this.playbackManager && window.Events) {
                if (this.onPlaybackStart) window.Events.off(this.playbackManager, 'playbackstart', this.onPlaybackStart);
                if (this.onPlaybackStop) window.Events.off(this.playbackManager, 'playbackstop', this.onPlaybackStop);
            }
        }
    }

    window._inputPlugin = inputPlugin;
    console.debug('[Media] inputPlugin class installed');
})();
