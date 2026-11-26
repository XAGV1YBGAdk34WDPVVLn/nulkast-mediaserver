(() => {
  const QUALITIES = [
    { value: 'auto', label: 'Auto' },
    { value: 'remux', label: 'Remux' },
    { value: '1080p', label: '1080p' },
    { value: '720p', label: '720p' },
    { value: '480p', label: '480p' },
    { value: 'cpu', label: 'CPU' },
  ];

  const state = {
    tab: 'home',
    movies: [],
    movieSort: 'added',
    movieGenre: 'All',
    shows: [],
    showSort: 'added',
    showGenre: 'All',
    activeShow: null,
    activeEpisodes: [],
    searchQuery: '',
    continueList: [],
    currentMedia: null,
    currentSessionId: null,
    currentMode: '',
    currentHwAccel: false,
    qualityIdx: 0,
    subTracks: [],
    activeSubIdx: -1, // index into subTracks, or -1 for Off
    activeSubOption: null,
    subOffsetSecs: 0,
    startOffset: 0,
    hls: null,
    retryCount: 0,
    scrubTarget: null,
    scrubHoldCount: 0,
    diagEnabled: true,
    progressTimer: null,
    statsTimer: null,
    upnextTimer: null,
    nextEpisode: null,
    lastFocusedKey: null,
    searchAbort: null,
    showAbort: null,
    gamepadRaf: null,
    lastGamepadNav: 0,
    prevPadButtons: [],
    hudHovered: false,
    stallTimer: null,
    stallRecoveries: 0,
    lastStatTick: 0,
    discoverEnabled: false,
    discoverMode: 'movies',
    discoverSort: 'popular',
    discoverFilter: 'All',
    discoverMovies: [],
    discoverTv: [],
    discoverTransfers: [],
    discoverJustQueued: new Set(),
    discoverLoaded: false,
    discoverPollTimer: null,
    discoverModalItem: null,
    discoverSelectedSeason: 1,
    discoverLastResults: [],
  };

  const $ = (id) => document.getElementById(id);
  const videoEl = $('video-el');
  const playerOverlay = $('player-overlay');
  const seekBar = $('seek-bar');
  const seekPlayed = $('seek-played');
  const seekBuffered = $('seek-buffered');
  const seekThumb = $('seek-thumb');
  const seekPreview = $('seek-preview');
  const playerTime = $('player-time');
  const playerStats = $('player-stats');
  const btnPlayPause = $('btn-play-pause');

  function fmtDuration(secs) {
    if (!secs || secs <= 0) return '';
    const h = Math.floor(secs / 3600);
    const m = Math.floor((secs % 3600) / 60);
    return h > 0 ? `${h}h ${m}m` : `${m}m`;
  }

  function fmtClock(secs) {
    const s = Math.max(0, Math.floor(secs || 0));
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const rem = s % 60;
    const pad = (n) => String(n).padStart(2, '0');
    return h > 0 ? `${h}:${pad(m)}:${pad(rem)}` : `${m}:${pad(rem)}`;
  }

  function escapeHtml(s) {
    return String(s || '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  }

  function monogram(title) {
    return String(title || '?').trim().split(/\s+/).slice(0, 2).map((w) => w[0] || '').join('').toUpperCase() || '?';
  }

  async function api(url, opts = {}) {
    const res = await fetch(url, opts);
    if (!res.ok) {
      let msg = `HTTP ${res.status}`;
      try { const j = await res.json(); if (j.error) msg = j.error; } catch {}
      throw new Error(msg);
    }
    return res.json();
  }

  function clientLog(level, message) {
    if (!state.diagEnabled && level !== 'error') return;
    fetch('/api/diagnostics/client-log', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ level, message }),
    }).catch(() => {});
  }

  function showSpinner(label = 'Loading stream...') {
    $('spinner-label').textContent = label;
    $('player-spinner').classList.remove('hidden');
  }
  function hideSpinner() {
    $('player-spinner').classList.add('hidden');
  }

  function showErrorBanner(msg) {
    hideSpinner();
    $('error-banner-msg').textContent = msg;
    $('player-error-banner').classList.remove('hidden');
    setTimeout(() => $('btn-err-retry')?.focus(), 40);
  }
  function hideErrorBanner() {
    $('player-error-banner').classList.add('hidden');
  }

  function clearUpNext() {
    if (state.upnextTimer) { clearInterval(state.upnextTimer); state.upnextTimer = null; }
    $('upnext-banner').classList.add('hidden');
  }

  function totalDuration() {
    const metaDur = state.currentMedia?.duration_secs || 0;
    const vidDur = isFinite(videoEl.duration) ? videoEl.duration : 0;
    return Math.max(metaDur, (state.currentMode === 'direct' ? 0 : state.startOffset) + vidDur, 1);
  }

  function absCurrentTime() {
    return (state.currentMode === 'direct' ? 0 : state.startOffset) + (videoEl.currentTime || 0);
  }

  function flushProgress(useBeacon = false) {
    if (!state.currentMedia) return;
    const absPos = absCurrentTime();
    if (absPos < 2) return;
    const payload = JSON.stringify({ position_secs: absPos });
    const url = `/api/media/${state.currentMedia.id}/progress`;
    if (useBeacon && navigator.sendBeacon) {
      navigator.sendBeacon(url, new Blob([payload], { type: 'application/json' }));
    } else {
      fetch(url, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: payload }).catch(() => {});
    }
  }

  function seekToAbs(targetSecs) {
    if (!state.currentMedia) return;
    const dur = totalDuration();
    const clamped = Math.max(0, Math.min(dur - 1, targetSecs));
    if (state.currentMode === 'direct') {
      videoEl.currentTime = clamped;
      return;
    }
    const relTarget = clamped - state.startOffset;
    let bufEnd = 0;
    if (videoEl.buffered && videoEl.buffered.length > 0) {
      bufEnd = videoEl.buffered.end(videoEl.buffered.length - 1);
    }
    const hlsDur = isFinite(videoEl.duration) ? videoEl.duration : 0;
    if (relTarget >= 0 && relTarget <= Math.max(bufEnd, hlsDur) - 1) {
      videoEl.currentTime = relTarget;
    } else {
      playMedia(state.currentMedia, QUALITIES[state.qualityIdx].value, clamped, false, true);
    }
  }

  let hideUiTimer = null;
  function pokePlayerUi() {
    if (playerOverlay.classList.contains('hidden')) return;
    const wasHidden = playerOverlay.classList.contains('idle-hide');
    playerOverlay.classList.remove('idle-hide');
    if (wasHidden) syncPlayerVisuals(true);
    clearTimeout(hideUiTimer);
    if (
      !videoEl.paused
      && !state.hudHovered
      && state.scrubTarget === null
      && $('player-error-banner').classList.contains('hidden')
      && $('sub-modal').classList.contains('hidden')
    ) {
      hideUiTimer = setTimeout(() => {
        if (!state.hudHovered && !videoEl.paused && state.scrubTarget === null) {
          playerOverlay.classList.add('idle-hide');
        }
      }, 5500);
    }
  }

  function clearStallWatchdog() {
    if (state.stallTimer) {
      clearTimeout(state.stallTimer);
      state.stallTimer = null;
    }
  }

  function armStallWatchdog() {
    clearStallWatchdog();
    if (playerOverlay.classList.contains('hidden') || !state.currentMedia) return;
    if (videoEl.paused && !videoEl.seeking) return;
    const snapTime = videoEl.currentTime;
    state.stallTimer = setTimeout(() => {
      if (playerOverlay.classList.contains('hidden') || !state.currentMedia) return;
      if (videoEl.paused && !videoEl.seeking) return;
      if (!videoEl.seeking && Math.abs(videoEl.currentTime - snapTime) > 0.25) return;
      state.stallRecoveries += 1;
      const pos = absCurrentTime();
      clientLog('warn', `Playback stalled at ${pos.toFixed(1)}s mode=${state.currentMode} (recovery #${state.stallRecoveries})`);
      if (state.hls && state.stallRecoveries === 1) {
        showSpinner('Recovering stream...');
        try {
          state.hls.recoverMediaError();
          state.hls.startLoad();
        } catch {}
        armStallWatchdog();
      } else if (state.currentMode === 'direct' && state.stallRecoveries === 1) {
        showSpinner('Reloading direct stream...');
        playMedia(state.currentMedia, QUALITIES[state.qualityIdx].value, pos, true, true);
      } else if (state.currentMode === 'direct' && state.stallRecoveries === 2) {
        showSpinner('Switching to RAM HLS stream...');
        playMedia(state.currentMedia, 'remux', pos, true, true);
      } else if (state.stallRecoveries <= 2) {
        showSpinner('Reloading stream...');
        playMedia(state.currentMedia, QUALITIES[state.qualityIdx].value, pos, true, true);
      } else {
        hideSpinner();
        showErrorBanner('Stream stalled while buffering/seeking. Try Retry, 720p, or CPU Mode.');
      }
    }, 12000);
  }

  function stepScrub(dir, isGamepad = false) {
    if (!state.currentMedia) return;
    pokePlayerUi();
    const total = totalDuration();
    if (state.scrubTarget === null) {
      state.scrubTarget = absCurrentTime();
      state.scrubHoldCount = 0;
    }
    state.scrubHoldCount += 1;
    const step = state.scrubHoldCount > 12 ? 60 : state.scrubHoldCount > 5 ? 30 : 10;
    state.scrubTarget = Math.max(0, Math.min(total - 1, state.scrubTarget + dir * step));
    const pct = Math.min(100, (state.scrubTarget / total) * 100);
    seekPlayed.style.width = `${pct.toFixed(2)}%`;
    seekThumb.style.left = `${pct.toFixed(2)}%`;
    seekPreview.textContent = `${fmtClock(state.scrubTarget)} (${isGamepad ? 'Press A' : 'Press Enter'})`;
    seekPreview.style.left = `${Math.max(8, Math.min(92, pct)).toFixed(2)}%`;
    seekPreview.classList.remove('hidden');
    playerTime.textContent = `${fmtClock(state.scrubTarget)} / ${fmtClock(total)}`;
  }

  function commitScrub() {
    if (state.scrubTarget === null) return false;
    const target = state.scrubTarget;
    cancelScrub();
    seekToAbs(target);
    pokePlayerUi();
    return true;
  }

  function cancelScrub() {
    state.scrubTarget = null;
    state.scrubHoldCount = 0;
    seekPreview.classList.add('hidden');
  }

  function syncPlayerVisuals(forceStats = false) {
    if (playerOverlay.classList.contains('hidden') || !state.currentMedia) return;
    if (playerOverlay.classList.contains('idle-hide') && !forceStats) return;

    const total = totalDuration();
    const absCur = absCurrentTime();
    let bufEndRel = 0;
    if (videoEl.buffered && videoEl.buffered.length > 0) {
      bufEndRel = videoEl.buffered.end(videoEl.buffered.length - 1);
    }
    const absBufEnd = (state.currentMode === 'direct' ? 0 : state.startOffset) + bufEndRel;
    const bufAhead = Math.max(0, bufEndRel - (videoEl.currentTime || 0));

    const playedPct = Math.min(100, (absCur / total) * 100);
    const bufStartPct = state.currentMode === 'direct' ? 0 : Math.min(100, (state.startOffset / total) * 100);
    const bufWidthPct = Math.max(0, Math.min(100 - bufStartPct, ((absBufEnd - (state.currentMode === 'direct' ? 0 : state.startOffset)) / total) * 100));

    seekBuffered.style.left = `${bufStartPct.toFixed(2)}%`;
    seekBuffered.style.width = `${bufWidthPct.toFixed(2)}%`;
    if (state.scrubTarget === null) {
      seekPlayed.style.width = `${playedPct.toFixed(2)}%`;
      seekThumb.style.left = `${playedPct.toFixed(2)}%`;
      seekBar.setAttribute('aria-valuenow', String(Math.round(playedPct)));
      playerTime.textContent = `${fmtClock(absCur)} / ${fmtClock(total)}`;
    }

    const now = performance.now();
    if (forceStats || now - state.lastStatTick > 950) {
      state.lastStatTick = now;
      const modeLabel = state.currentMode === 'direct'
        ? 'DIRECT PLAY'
        : state.currentMode === 'remux'
          ? 'ZERO-COPY REMUX'
          : state.currentHwAccel ? 'INTEL VA-API HW' : 'CPU HLS';
      const diagExtra = state.diagEnabled
        ? ` • ${videoEl.videoWidth || 0}x${videoEl.videoHeight || 0} • ready=${videoEl.readyState}`
        : '';
      playerStats.textContent = `${modeLabel} • Buffer: ${bufAhead.toFixed(0)}s ahead${diagExtra}`;
    }
  }

  function makeMediaCard(item) {
    const btn = document.createElement('button');
    btn.className = 'card focusable';
    btn.dataset.focusKey = `media-${item.id}`;
    const pct = item.duration_secs > 10 ? Math.min(100, (item.position_secs / item.duration_secs) * 100) : 0;
    const remMins = item.duration_secs > 60 && item.position_secs > 10
      ? Math.max(1, Math.round((item.duration_secs - item.position_secs) / 60))
      : 0;
    const resTag = item.height >= 2000 ? '4K' : item.height >= 1000 ? '1080p' : item.height > 0 ? `${item.height}p` : '';
    const subTitle = item.show_name ? `${item.show_name} • ${item.title}` : item.title;
    const posterKey = encodeURIComponent(`${item.title}-${item.year || ''}`);

    btn.innerHTML = `
      <div class="card-thumb-wrap poster-ratio">
        <span class="card-fallback">${escapeHtml(monogram(item.show_name || item.title))}</span>
        <img class="card-thumb" loading="lazy" decoding="async" src="/api/media/${item.id}/poster?k=${posterKey}" alt="" onerror="this.style.display='none'" />
      </div>
      <div class="card-body">
        <div class="card-title">${escapeHtml(subTitle)}</div>
        <div class="card-meta">
          ${item.year ? `<span class="tag">${item.year}</span>` : ''}
          ${resTag ? `<span class="tag">${resTag}</span>` : ''}
          ${remMins > 0 ? `<span>${remMins}m left</span>` : (item.duration_secs ? `<span>${fmtDuration(item.duration_secs)}</span>` : '')}
        </div>
        ${pct > 1 ? `<div class="progress-bar"><div class="progress-fill" style="width:${pct.toFixed(1)}%"></div></div>` : ''}
      </div>
    `;
    btn.onclick = () => {
      state.lastFocusedKey = btn.dataset.focusKey;
      playMedia(item);
    };
    return btn;
  }

  function makeShowCard(show) {
    const btn = document.createElement('button');
    btn.className = 'card focusable';
    btn.dataset.focusKey = `show-${show.show_name}`;
    btn.innerHTML = `
      <div class="card-thumb-wrap poster-ratio">
        <span class="card-fallback">${escapeHtml(monogram(show.show_name))}</span>
        <img class="card-thumb" loading="lazy" decoding="async" src="/api/shows/${encodeURIComponent(show.show_name)}/poster" alt="" onerror="this.onerror=null;this.src='/api/media/${show.latest_id}/thumb'" />
      </div>
      <div class="card-body">
        <div class="card-title">${escapeHtml(show.show_name)}</div>
        <div class="card-meta">
          <span class="tag">${show.season_count} ${show.season_count === 1 ? 'Season' : 'Seasons'}</span>
          <span class="tag">${show.episode_count} Eps</span>
        </div>
      </div>
    `;
    btn.onclick = () => {
      state.lastFocusedKey = btn.dataset.focusKey;
      openShow(show.show_name);
    };
    return btn;
  }

  function renderSkeletons(container, count = 8) {
    const frag = document.createDocumentFragment();
    for (let i = 0; i < count; i++) {
      const div = document.createElement('div');
      div.className = 'skeleton-card';
      frag.appendChild(div);
    }
    container.innerHTML = '';
    container.appendChild(frag);
  }

  function restoreFocusByKey(key) {
    if (!key) return false;
    const el = document.querySelector(`[data-focus-key="${CSS.escape(key)}"]`);
    if (el && el.offsetParent !== null) {
      el.focus();
      return true;
    }
    return false;
  }

  async function refreshDiagnostics() {
    try {
      const d = await api('/api/diagnostics');
      state.diagEnabled = !!d.enabled;
      $('btn-diag').textContent = state.diagEnabled ? 'Diag: ON' : 'Diag: OFF';
      const lines = [
        `Enabled: ${d.enabled} | VA-API: ${d.hw_vaapi} (${d.vaapi_device || 'none'}) | Scratch: ${d.scratch_dir}`,
        `Active Sessions (${(d.active_sessions || []).length}):`,
        ...(d.active_sessions || []).map(
          (s) => `  - ${s.id.slice(0, 8)} media=${s.media_id} mode=${s.mode} segs=${s.segments_ready} idle=${s.idle_secs}s${s.last_stderr.length ? ' stderr=' + s.last_stderr.slice(-2).join(' | ') : ''}`
        ),
        `Recent Events:`,
        ...(d.events || []).slice(0, 20).map((e) => `  [${e.level.toUpperCase()}/${e.category}] ${e.message}`),
      ];
      $('diag-output').textContent = lines.join('\n');
    } catch (e) {
      $('diag-output').textContent = `Diagnostics error: ${e.message}`;
    }
  }

  function syncUrl(push = false) {
    let hash = `#/${state.tab}`;
    if (state.tab === 'movies') {
      const p = new URLSearchParams();
      if (state.movieSort !== 'added') p.set('sort', state.movieSort);
      if (state.movieGenre !== 'All') p.set('genre', state.movieGenre);
      const qs = p.toString();
      if (qs) hash += `?${qs}`;
    } else if (state.tab === 'shows') {
      if (state.activeShow) {
        hash = `#/shows/${encodeURIComponent(state.activeShow)}`;
      } else {
        const p = new URLSearchParams();
        if (state.showSort !== 'added') p.set('sort', state.showSort);
        if (state.showGenre !== 'All') p.set('genre', state.showGenre);
        const qs = p.toString();
        if (qs) hash += `?${qs}`;
      }
    } else if (state.tab === 'search' && state.searchQuery) {
      hash = `#/search?q=${encodeURIComponent(state.searchQuery)}`;
    }
    if (window.location.hash !== hash) {
      if (push) window.location.hash = hash;
      else history.replaceState(null, '', hash);
    }
  }

  async function applyRouteFromUrl() {
    const raw = (window.location.hash || '#/home').replace(/^#\/?/, '');
    const [pathPart, qsPart] = raw.split('?');
    const segs = pathPart.split('/').filter(Boolean);
    const params = new URLSearchParams(qsPart || '');
    const route = segs[0] || 'home';

    if (route === 'movies') {
      state.movieSort = params.get('sort') || 'added';
      state.movieGenre = params.get('genre') || 'All';
      state.activeShow = null;
      switchTab('movies', false);
      renderMovies();
    } else if (route === 'shows') {
      state.showSort = params.get('sort') || 'added';
      state.showGenre = params.get('genre') || 'All';
      if (segs[1]) {
        await openShow(decodeURIComponent(segs[1]), false);
      } else {
        state.activeShow = null;
        switchTab('shows', false);
        renderShows();
      }
    } else if (route === 'discover') {
      state.activeShow = null;
      switchTab('discover', false);
      if (state.discoverEnabled) loadDiscoverCatalog(false);
    } else if (route === 'search') {
      const q = params.get('q') || '';
      state.searchQuery = q;
      $('search-input').value = q;
      if (q) await runSearch(q, false);
      else switchTab('home', false);
    } else {
      state.activeShow = null;
      switchTab('home', false);
    }
  }

  async function loadAll() {
    renderSkeletons($('home-movies-grid'), 8);
    try {
      const [health, movies, shows, cont] = await Promise.all([
        api('/api/health'),
        api('/api/movies'),
        api('/api/shows'),
        api('/api/continue'),
      ]);
      if (health.server_name && $('brand-name')) {
        $('brand-name').textContent = health.server_name.toUpperCase();
        document.title = health.server_name;
      }
      state.diagEnabled = !!health.diagnostics_enabled;
      state.discoverEnabled = !!health.discover_enabled;
      $('nav-discover')?.classList.toggle('hidden', !state.discoverEnabled);
      $('btn-diag').textContent = state.diagEnabled ? 'Diag: ON' : 'Diag: OFF';
      $('hw-badge').textContent = health.hw_vaapi ? 'INTEL VA-API + RAM HLS' : 'CPU + RAM HLS';
      state.movies = movies.items || [];
      state.shows = shows.shows || [];
      state.continueList = cont.items || [];
      renderHome();
      await applyRouteFromUrl();
    } catch (e) {
      $('hw-badge').textContent = 'OFFLINE';
      $('home-movies-grid').innerHTML = `<div class="empty-state">Unable to connect to server (${escapeHtml(e.message)})</div>`;
    }
  }

  async function refreshContinueOnly() {
    try {
      const cont = await api('/api/continue');
      state.continueList = cont.items || [];
      renderContinueSection();
    } catch {}
  }

  function renderContinueSection() {
    const cSec = $('continue-section');
    const cGrid = $('continue-grid');
    cGrid.innerHTML = '';
    if (state.continueList.length > 0) {
      cSec.classList.remove('hidden');
      const frag = document.createDocumentFragment();
      state.continueList.forEach((m) => frag.appendChild(makeMediaCard(m)));
      cGrid.appendChild(frag);
    } else {
      cSec.classList.add('hidden');
    }
  }

  function renderHome() {
    renderContinueSection();

    const mGrid = $('home-movies-grid');
    mGrid.innerHTML = '';
    if (state.movies.length === 0) {
      mGrid.innerHTML = '<div class="empty-state">No movies indexed yet — click Scan to index your library.</div>';
    } else {
      const frag = document.createDocumentFragment();
      state.movies.slice().sort((a, b) => (b.mtime || 0) - (a.mtime || 0)).slice(0, 18)
        .forEach((m) => frag.appendChild(makeMediaCard(m)));
      mGrid.appendChild(frag);
    }

    const sGrid = $('home-shows-grid');
    sGrid.innerHTML = '';
    if (state.shows.length === 0) {
      sGrid.innerHTML = '<div class="empty-state">No TV shows found yet.</div>';
    } else {
      const frag = document.createDocumentFragment();
      state.shows.slice(0, 18).forEach((s) => frag.appendChild(makeShowCard(s)));
      sGrid.appendChild(frag);
    }
  }

  function renderMovies(restoreChipGenre = null) {
    const counts = new Map();
    state.movies.forEach((m) => {
      (m.genres || '').split(',').map((g) => g.trim()).filter(Boolean).forEach((g) => {
        counts.set(g, (counts.get(g) || 0) + 1);
      });
    });

    const genreRow = $('genre-chips');
    if (genreRow) {
      const frag = document.createDocumentFragment();
      const lbl = document.createElement('span');
      lbl.className = 'chip-label';
      lbl.textContent = 'Category:';
      frag.appendChild(lbl);

      const allBtn = document.createElement('button');
      allBtn.className = `chip focusable${state.movieGenre === 'All' ? ' active' : ''}`;
      allBtn.dataset.focusKey = 'mgenre-All';
      allBtn.textContent = `All (${state.movies.length})`;
      allBtn.onclick = () => { state.movieGenre = 'All'; renderMovies('All'); syncUrl(true); };
      frag.appendChild(allBtn);

      Array.from(counts.entries())
        .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
        .forEach(([genre, cnt]) => {
          const b = document.createElement('button');
          b.className = `chip focusable${state.movieGenre === genre ? ' active' : ''}`;
          b.dataset.focusKey = `mgenre-${genre}`;
          b.textContent = `${genre} (${cnt})`;
          b.onclick = () => { state.movieGenre = genre; renderMovies(genre); syncUrl(true); };
          frag.appendChild(b);
        });
      genreRow.innerHTML = '';
      genreRow.appendChild(frag);
    }

    document.querySelectorAll('.chip[data-sort]').forEach((b) => {
      b.classList.toggle('active', b.dataset.sort === state.movieSort);
    });

    let list = state.movies.slice();
    if (state.movieGenre !== 'All') {
      const target = state.movieGenre.toLowerCase();
      list = list.filter((m) => (m.genres || '').toLowerCase().split(',').map((s) => s.trim()).includes(target));
    }
    if (state.movieSort === 'added') {
      list.sort((a, b) => (b.mtime || 0) - (a.mtime || 0) || a.title.localeCompare(b.title));
    } else if (state.movieSort === 'year_desc') {
      list.sort((a, b) => (b.year || 0) - (a.year || 0) || a.title.localeCompare(b.title));
    } else if (state.movieSort === 'year_asc') {
      list.sort((a, b) => (a.year || 9999) - (b.year || 9999) || a.title.localeCompare(b.title));
    } else if (state.movieSort === 'za') {
      list.sort((a, b) => b.title.localeCompare(a.title));
    } else {
      list.sort((a, b) => a.title.localeCompare(b.title));
    }

    $('movies-heading').textContent = state.movieGenre === 'All'
      ? `All Movies (${list.length})`
      : `${state.movieGenre} Movies (${list.length})`;

    const g = $('movies-grid');
    g.innerHTML = '';
    if (list.length === 0) {
      g.innerHTML = '<div class="empty-state">No movies match this filter.</div>';
    } else {
      const frag = document.createDocumentFragment();
      list.forEach((m) => frag.appendChild(makeMediaCard(m)));
      g.appendChild(frag);
    }
    if (restoreChipGenre) restoreFocusByKey(`mgenre-${restoreChipGenre}`);
  }

  function renderShows(restoreChipGenre = null) {
    state.activeShow = null;
    $('btn-back-shows').classList.add('hidden');
    $('episodes-list').classList.add('hidden');
    $('shows-toolbar')?.classList.remove('hidden');

    const counts = new Map();
    state.shows.forEach((s) => {
      (s.genres || '').split(',').map((g) => g.trim()).filter(Boolean).forEach((g) => {
        counts.set(g, (counts.get(g) || 0) + 1);
      });
    });

    const genreRow = $('show-genre-chips');
    if (genreRow) {
      const frag = document.createDocumentFragment();
      const lbl = document.createElement('span');
      lbl.className = 'chip-label';
      lbl.textContent = 'Category:';
      frag.appendChild(lbl);

      const allBtn = document.createElement('button');
      allBtn.className = `chip focusable${state.showGenre === 'All' ? ' active' : ''}`;
      allBtn.dataset.focusKey = 'sgenre-All';
      allBtn.textContent = `All (${state.shows.length})`;
      allBtn.onclick = () => { state.showGenre = 'All'; renderShows('All'); syncUrl(true); };
      frag.appendChild(allBtn);

      Array.from(counts.entries())
        .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
        .forEach(([genre, cnt]) => {
          const b = document.createElement('button');
          b.className = `chip focusable${state.showGenre === genre ? ' active' : ''}`;
          b.dataset.focusKey = `sgenre-${genre}`;
          b.textContent = `${genre} (${cnt})`;
          b.onclick = () => { state.showGenre = genre; renderShows(genre); syncUrl(true); };
          frag.appendChild(b);
        });
      genreRow.innerHTML = '';
      genreRow.appendChild(frag);
    }

    document.querySelectorAll('.chip[data-show-sort]').forEach((b) => {
      b.classList.toggle('active', b.dataset.showSort === state.showSort);
    });

    let list = state.shows.slice();
    if (state.showGenre !== 'All') {
      const target = state.showGenre.toLowerCase();
      list = list.filter((s) => (s.genres || '').toLowerCase().split(',').map((x) => x.trim()).includes(target));
    }
    if (state.showSort === 'added') {
      list.sort((a, b) => (b.latest_mtime || 0) - (a.latest_mtime || 0) || a.show_name.localeCompare(b.show_name));
    } else if (state.showSort === 'episodes') {
      list.sort((a, b) => (b.episode_count || 0) - (a.episode_count || 0) || a.show_name.localeCompare(b.show_name));
    } else if (state.showSort === 'za') {
      list.sort((a, b) => b.show_name.localeCompare(a.show_name));
    } else {
      list.sort((a, b) => a.show_name.localeCompare(b.show_name));
    }

    $('shows-heading').textContent = state.showGenre === 'All'
      ? `TV Shows (${list.length})`
      : `${state.showGenre} TV Shows (${list.length})`;

    const g = $('shows-grid');
    g.classList.remove('hidden');
    g.innerHTML = '';
    if (list.length === 0) {
      g.innerHTML = '<div class="empty-state">No TV shows match this filter.</div>';
    } else {
      const frag = document.createDocumentFragment();
      list.forEach((s) => frag.appendChild(makeShowCard(s)));
      g.appendChild(frag);
    }
    if (restoreChipGenre) restoreFocusByKey(`sgenre-${restoreChipGenre}`);
  }

  async function openShow(showName, updateUrl = true) {
    if (state.showAbort) state.showAbort.abort();
    state.showAbort = new AbortController();

    state.activeShow = showName;
    switchTab('shows', false);
    if (updateUrl) syncUrl(true);
    $('shows-heading').textContent = showName;
    $('btn-back-shows').classList.remove('hidden');
    $('shows-toolbar')?.classList.add('hidden');
    $('shows-grid').classList.add('hidden');
    const list = $('episodes-list');
    list.classList.remove('hidden');
    list.innerHTML = '<div class="empty-state">Loading episodes...</div>';

    try {
      const data = await api(`/api/shows/${encodeURIComponent(showName)}/episodes`, { signal: state.showAbort.signal });
      state.activeEpisodes = data.episodes || [];
      list.innerHTML = '';
      if (state.activeEpisodes.length === 0) {
        list.innerHTML = '<div class="empty-state">No episodes found for this show.</div>';
        return;
      }
      const frag = document.createDocumentFragment();
      state.activeEpisodes.forEach((ep) => {
        const row = document.createElement('button');
        row.className = 'episode-row focusable';
        row.dataset.focusKey = `ep-${ep.id}`;
        const pct = ep.duration_secs > 10 ? Math.min(100, (ep.position_secs / ep.duration_secs) * 100) : 0;
        row.innerHTML = `
          <div class="ep-left">
            <img class="ep-thumb" loading="lazy" decoding="async" src="/api/media/${ep.id}/thumb" alt="" onerror="this.style.display='none'" />
            <div>
              <div class="card-title">${escapeHtml(ep.title)}</div>
              <div class="card-meta">
                <span class="tag">${escapeHtml(ep.video_codec || 'video')}</span>
                <span class="tag">${escapeHtml(ep.audio_codec || 'audio')}</span>
                <span>${fmtDuration(ep.duration_secs)}</span>
                ${pct > 1 ? `<span>• ${pct.toFixed(0)}% watched</span>` : ''}
              </div>
            </div>
          </div>
          <span class="pill">▶ Play</span>
        `;
        row.onclick = () => {
          state.lastFocusedKey = row.dataset.focusKey;
          playMedia(ep);
        };
        frag.appendChild(row);
      });
      list.appendChild(frag);
      list.querySelector('.episode-row')?.focus();
    } catch (e) {
      if (e.name !== 'AbortError') {
        list.innerHTML = `<div class="empty-state">Error loading episodes: ${escapeHtml(e.message)}</div>`;
      }
    }
  }

  function fmtBytes(b) {
    if (!b || b <= 0) return '';
    const gb = b / (1024 * 1024 * 1024);
    if (gb >= 1) return `${gb.toFixed(2)} GB`;
    return `${Math.round(b / (1024 * 1024))} MB`;
  }

  // Unified Discover module (shared with standalone/Torrent Discover view via discover-shared.js)
  const discoverEngine = window.NulKastDiscover ? window.NulKastDiscover.create({
    apiBase: '/api/discover',
    apiFetch: api,
    escapeHtml,
    fmtBytes,
    renderSkeletons,
    setLastFocusKey: (k) => { state.lastFocusedKey = k; },
    onRestoreFocus: (k) => restoreFocusByKey(k || state.lastFocusedKey),
    onPlayLibraryItem: (item, kind) => {
      if (kind === 'tv' && item.libraryShowName) {
        openShow(item.libraryShowName, true);
      } else if (item.libraryMediaId) {
        const found = state.movies.find((m) => m.id === item.libraryMediaId);
        if (found) playMedia(found);
      }
    },
  }) : null;
  if (discoverEngine) discoverEngine.bindEvents();

  async function loadDiscoverCatalog(force = false) {
    if (!discoverEngine) return;
    discoverEngine.state.enabled = state.discoverEnabled;
    await discoverEngine.loadCatalog(force);
  }

  function closeDiscoverModal() {
    if (discoverEngine) discoverEngine.closeModal();
  }

  function switchTab(tab, updateUrl = true) {
    state.tab = tab;
    document.querySelectorAll('.nav-btn[data-tab]').forEach((b) => {
      const isAct = b.dataset.tab === tab;
      b.classList.toggle('active', isAct);
      b.setAttribute('aria-selected', isAct ? 'true' : 'false');
    });
    document.querySelectorAll('.view').forEach((v) => v.classList.remove('active'));
    const el = $(`view-${tab}`);
    if (el) el.classList.add('active');
    if (updateUrl) syncUrl(true);
  }

  async function stopActiveSession(sendDelete = true, preserveRetries = false) {
    clearUpNext();
    clearStallWatchdog();
    if (!preserveRetries) state.stallRecoveries = 0;
    state.hudHovered = false;
    if (state.progressTimer) { clearInterval(state.progressTimer); state.progressTimer = null; }
    if (state.statsTimer) { clearInterval(state.statsTimer); state.statsTimer = null; }
    if (state.hls) { state.hls.destroy(); state.hls = null; }
    videoEl.pause();
    videoEl.removeAttribute('src');
    videoEl.querySelectorAll('track').forEach((t) => t.remove());
    videoEl.load();
    const sid = state.currentSessionId;
    state.currentSessionId = null;
    if (sid && sendDelete) {
      fetch(`/api/hls/${sid}`, { method: 'DELETE' }).catch(() => {});
    }
    return sid;
  }

  function applyCueOffset(trackObj, deltaSecs) {
    if (!trackObj || !trackObj.cues || !deltaSecs) return;
    for (let i = 0; i < trackObj.cues.length; i++) {
      const c = trackObj.cues[i];
      c.startTime = Math.max(0, c.startTime + deltaSecs);
      c.endTime = Math.max(c.startTime + 0.1, c.endTime + deltaSecs);
    }
  }

  function applySubtitleOption(opt) {
    videoEl.querySelectorAll('track').forEach((t) => t.remove());
    state.activeSubOption = opt || null;
    if (!opt || !state.currentMedia) {
      state.activeSubIdx = -1;
      $('btn-sub').textContent = 'Sub: Off';
      return;
    }
    $('btn-sub').textContent = `Sub: ${(opt.language || 'ON').toUpperCase().slice(0, 6)}`;
    const track = document.createElement('track');
    track.kind = 'subtitles';
    track.label = opt.label || opt.language || 'Subtitles';
    track.src = opt.url;
    track.default = true;
    track.addEventListener('load', () => {
      const tt = videoEl.textTracks[0];
      if (tt) {
        tt.mode = 'showing';
        if (state.subOffsetSecs !== 0) applyCueOffset(tt, state.subOffsetSecs);
      }
    });
    videoEl.appendChild(track);
    if (videoEl.textTracks[0]) videoEl.textTracks[0].mode = 'showing';
  }

  function applySubtitleTrack(subIdx) {
    state.activeSubIdx = subIdx;
    if (subIdx < 0 || subIdx >= state.subTracks.length || !state.currentMedia) {
      applySubtitleOption(null);
      return;
    }
    const sub = state.subTracks[subIdx];
    applySubtitleOption({
      id: `emb_${sub.index}`,
      label: sub.title || sub.language || 'Embedded',
      language: sub.language || 'ON',
      source: 'embedded',
      url: `/api/stream/${state.currentMedia.id}/subtitles/${sub.index}`,
    });
  }

  function setSubModalStatus(msg, kind = '') {
    const el = $('sub-modal-status');
    if (!el) return;
    el.textContent = msg || '';
    el.className = `dmodal-status${kind ? ' ' + kind : ''}`;
  }

  function closeSubtitleModal() {
    $('sub-modal').classList.add('hidden');
    pokePlayerUi();
    $('btn-sub')?.focus();
  }

  async function openSubtitleModal() {
    if (!state.currentMedia) return;
    clearTimeout(hideUiTimer);
    playerOverlay.classList.remove('idle-hide');
    $('sub-offset-label').textContent = `${state.subOffsetSecs >= 0 ? '+' : ''}${state.subOffsetSecs.toFixed(1)}s`;
    $('sub-modal').classList.remove('hidden');
    setSubModalStatus('Loading available subtitle tracks...');
    const listEl = $('sub-modal-list');
    listEl.innerHTML = '';

    try {
      const data = await api(`/api/media/${state.currentMedia.id}/subtitles`);
      const tracks = data.tracks || [];
      setSubModalStatus(
        tracks.length > 0
          ? `${tracks.length} local track(s) available — select one or click Find Online Subtitles.`
          : 'No local subtitles found — click Find Online Subtitles to search OpenSubtitles.'
      );
      renderLocalSubtitleList(tracks);
      listEl.querySelector('button')?.focus() || $('btn-sub-search-online')?.focus();
    } catch (e) {
      setSubModalStatus(`Could not load subtitles: ${e.message}`, 'error');
    }
  }

  function renderLocalSubtitleList(tracks) {
    const listEl = $('sub-modal-list');
    listEl.innerHTML = '';
    const frag = document.createDocumentFragment();

    const offRow = document.createElement('button');
    offRow.className = `dmodal-result-row focusable${!state.activeSubOption ? ' queued' : ''}`;
    offRow.innerHTML = `
      <div style="min-width:0; flex:1;"><div class="dmodal-result-name">Subtitles Off</div></div>
      <span class="pill">${!state.activeSubOption ? '✓ Active' : 'Disable'}</span>
    `;
    offRow.onclick = () => { applySubtitleOption(null); closeSubtitleModal(); };
    frag.appendChild(offRow);

    tracks.forEach((tr) => {
      const isAct = state.activeSubOption && state.activeSubOption.id === tr.id;
      const row = document.createElement('button');
      row.className = `dmodal-result-row focusable${isAct ? ' queued' : ''}`;
      const badge = tr.source === 'downloaded' ? 'Downloaded' : tr.source === 'sidecar' ? 'Sidecar File' : 'Embedded';
      row.innerHTML = `
        <div style="min-width:0; flex:1;">
          <div class="dmodal-result-name">${escapeHtml(tr.label)}</div>
          <div class="dmodal-result-meta"><span class="tag">${badge}</span></div>
        </div>
        <span class="pill">${isAct ? '✓ Active' : 'Select'}</span>
      `;
      row.onclick = () => { applySubtitleOption(tr); closeSubtitleModal(); };
      frag.appendChild(row);
    });
    listEl.appendChild(frag);
  }

  async function searchSubtitlesOnline() {
    if (!state.currentMedia) return;
    setSubModalStatus('🔍 Searching OpenSubtitles for English subtitles...');
    const listEl = $('sub-modal-list');
    listEl.innerHTML = '';
    try {
      const data = await api(`/api/media/${state.currentMedia.id}/subtitles/search?lang=eng`);
      const results = data.results || [];
      if (results.length === 0) {
        setSubModalStatus('No online English subtitles found for this title.', 'error');
        return;
      }
      setSubModalStatus(`Found ${results.length} online subtitle(s) — highlight a row and press A / Click to download & enable.`, 'ok');
      const frag = document.createDocumentFragment();
      results.forEach((r) => {
        const row = document.createElement('button');
        row.className = 'dmodal-result-row focusable';
        row.innerHTML = `
          <div style="min-width:0; flex:1;">
            <div class="dmodal-result-name">${escapeHtml(r.filename)}</div>
            <div class="dmodal-result-meta">
              <span class="tag">${escapeHtml(r.language)}</span>
              <span class="tag">${escapeHtml(r.format.toUpperCase())}</span>
              ${r.hearing_impaired ? '<span class="tag">SDH</span>' : ''}
              <span class="tag" style="color:#34d399;">↓ ${r.downloads.toLocaleString()}</span>
            </div>
          </div>
          <span class="pill">⬇ Use</span>
        `;
        row.onclick = async () => {
          row.disabled = true;
          row.querySelector('.pill').textContent = '⏳ Loading...';
          setSubModalStatus(`Downloading & converting "${r.filename}"...`);
          try {
            const dl = await api(`/api/media/${state.currentMedia.id}/subtitles/download`, {
              method: 'POST',
              headers: { 'Content-Type': 'application/json' },
              body: JSON.stringify({
                sub_id: r.sub_id,
                download_url: r.download_url,
                filename: r.filename,
                language: r.language,
              }),
            });
            if (dl.track) {
              applySubtitleOption(dl.track);
              setSubModalStatus(`✓ Loaded "${r.filename}"!`, 'ok');
              closeSubtitleModal();
            }
          } catch (e) {
            setSubModalStatus(`Subtitle download failed: ${e.message}`, 'error');
            row.disabled = false;
            row.querySelector('.pill').textContent = '⬇ Retry';
          }
        };
        frag.appendChild(row);
      });
      listEl.appendChild(frag);
      listEl.querySelector('button')?.focus();
    } catch (e) {
      setSubModalStatus(`Online search failed: ${e.message}`, 'error');
    }
  }

  async function computeNextEpisode(item) {
    state.nextEpisode = null;
    $('btn-next-ep').classList.add('hidden');
    if (item.kind !== 'episode' || !item.show_name) return;
    let eps = state.activeEpisodes;
    if (!eps.length || eps[0]?.show_name?.toLowerCase() !== item.show_name.toLowerCase()) {
      try {
        const d = await api(`/api/shows/${encodeURIComponent(item.show_name)}/episodes`);
        eps = d.episodes || [];
        state.activeEpisodes = eps;
      } catch {
        return;
      }
    }
    const idx = eps.findIndex((e) => e.id === item.id);
    if (idx >= 0 && idx + 1 < eps.length) {
      state.nextEpisode = eps[idx + 1];
      $('btn-next-ep').classList.remove('hidden');
    }
  }

  async function playMedia(item, qualityOverride = null, startOverride = null, isRetry = false, preserveSub = false) {
    const prevSid = await stopActiveSession(false, isRetry);
    if (!isRetry) {
      state.retryCount = 0;
      state.stallRecoveries = 0;
    }

    const sameMedia = state.currentMedia && state.currentMedia.id === item.id;
    const savedSubIdx = (preserveSub || isRetry || sameMedia) ? state.activeSubIdx : -1;
    state.currentMedia = item;

    if (qualityOverride) {
      const qi = QUALITIES.findIndex((q) => q.value === qualityOverride);
      if (qi >= 0) state.qualityIdx = qi;
    }
    const quality = QUALITIES[state.qualityIdx].value;
    $('btn-quality').textContent = `Quality: ${QUALITIES[state.qualityIdx].label}`;

    const startSecs = startOverride !== null ? startOverride : (item.position_secs > 15 && !item.watched ? item.position_secs : 0);
    state.startOffset = startSecs;

    cancelScrub();
    hideErrorBanner();
    document.documentElement.classList.add('player-open');
    document.body.classList.add('player-open');
    playerOverlay.classList.remove('hidden');
    $('player-title').textContent = item.show_name ? `${item.show_name} — ${item.title}` : item.title;
    playerStats.textContent = startSecs > 15 ? `Resuming at ${fmtClock(startSecs)}...` : 'Starting stream in RAM...';
    showSpinner(startSecs > 15 ? `Resuming at ${fmtClock(startSecs)}...` : 'Starting stream...');
    pokePlayerUi();
    setTimeout(() => btnPlayPause.focus(), 60);

    try {
      const subs = JSON.parse(item.subtitles_json || '[]');
      state.subTracks = subs.filter((s) => s.is_text);
    } catch {
      state.subTracks = [];
    }
    $('btn-sub').classList.toggle('hidden', state.subTracks.length === 0);
    applySubtitleTrack(savedSubIdx);

    computeNextEpisode(item);

    let resp;
    try {
      resp = await api(`/api/stream/${item.id}/start`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          start_secs: startSecs,
          quality,
          previous_session_id: prevSid,
        }),
      });
    } catch (e) {
      clientLog('error', `Start failed media=${item.id}: ${e.message}`);
      showErrorBanner(`Stream failed to start: ${e.message}`);
      return;
    }

    state.currentSessionId = resp.session_id;
    state.currentMode = resp.mode;
    state.currentHwAccel = !!resp.hw_accel;
    clientLog('info', `Playing media=${item.id} mode=${resp.mode} hw=${resp.hw_accel} start=${startSecs.toFixed(0)}s`);

    if (resp.mode === 'direct') {
      const bust = `${resp.stream_url}${resp.stream_url.includes('?') ? '&' : '?'}r=${Date.now()}`;
      if (startSecs > 0) {
        videoEl.addEventListener('loadedmetadata', () => {
          videoEl.addEventListener('seeked', () => {
            hideSpinner();
            videoEl.play().catch((err) => clientLog('warn', `Autoplay blocked: ${err.message}`));
          }, { once: true });
          videoEl.currentTime = startSecs;
          armStallWatchdog();
        }, { once: true });
        videoEl.src = bust;
      } else {
        videoEl.src = bust;
        videoEl.play().catch((err) => clientLog('warn', `Autoplay blocked: ${err.message}`));
      }
      armStallWatchdog();
    } else if (window.Hls && window.Hls.isSupported()) {
      const hls = new window.Hls({
        maxBufferLength: 60,
        maxMaxBufferLength: 180,
        backBufferLength: 30,
        lowLatencyMode: false,
      });
      state.hls = hls;
      hls.loadSource(resp.stream_url);
      hls.attachMedia(videoEl);
      hls.on(window.Hls.Events.MANIFEST_PARSED, () => {
        videoEl.play().catch((err) => clientLog('warn', `Autoplay blocked: ${err.message}`));
      });
      hls.on(window.Hls.Events.ERROR, (_ev, data) => {
        if (data.fatal) {
          clientLog('error', `HLS fatal (${data.type}/${data.details}) retry=${state.retryCount}`);
          if (state.retryCount < 2 && state.currentMedia) {
            state.retryCount += 1;
            const pos = absCurrentTime();
            playMedia(state.currentMedia, QUALITIES[state.qualityIdx].value, pos, true, true);
          } else {
            showErrorBanner(`Playback error (${data.details || data.type}). Try 720p or CPU mode.`);
          }
        }
      });
    } else {
      videoEl.src = resp.stream_url;
      videoEl.play().catch(() => {});
    }

    syncPlayerVisuals(true);
    state.statsTimer = setInterval(() => syncPlayerVisuals(false), 350);
    state.progressTimer = setInterval(() => {
      if (!videoEl.paused) flushProgress(false);
    }, 5000);
  }

  // Video element lifecycle hooks
  videoEl.addEventListener('waiting', () => { showSpinner('Buffering...'); armStallWatchdog(); });
  videoEl.addEventListener('seeking', () => { showSpinner('Seeking...'); armStallWatchdog(); });
  videoEl.addEventListener('seeked', () => { if (!videoEl.paused) hideSpinner(); });
  videoEl.addEventListener('playing', () => { clearStallWatchdog(); state.stallRecoveries = 0; hideSpinner(); btnPlayPause.textContent = '⏸ Pause'; pokePlayerUi(); });
  videoEl.addEventListener('timeupdate', () => { if (!videoEl.paused && videoEl.readyState >= 3) clearStallWatchdog(); });
  videoEl.addEventListener('canplay', () => hideSpinner());
  videoEl.addEventListener('pause', () => { clearStallWatchdog(); btnPlayPause.textContent = '▶ Play'; flushProgress(false); pokePlayerUi(); });
  videoEl.addEventListener('ended', () => {
    flushProgress(false);
    if (state.nextEpisode) {
      let secs = 8;
      $('upnext-title').textContent = `${state.nextEpisode.title} (in ${secs}s)`;
      $('upnext-banner').classList.remove('hidden');
      $('btn-upnext-play').focus();
      state.upnextTimer = setInterval(() => {
        secs -= 1;
        if (secs <= 0) {
          clearUpNext();
          if (state.nextEpisode) playMedia(state.nextEpisode, null, 0);
        } else {
          $('upnext-title').textContent = `${state.nextEpisode.title} (in ${secs}s)`;
        }
      }, 1000);
    }
  });
  window.addEventListener('pagehide', () => flushProgress(true));

  // Player button handlers
  $('btn-close-player').onclick = async () => {
    flushProgress(false);
    cancelScrub();
    hideErrorBanner();
    hideSpinner();
    await stopActiveSession(true);
    document.documentElement.classList.remove('player-open');
    document.body.classList.remove('player-open');
    playerOverlay.classList.add('hidden');
    await refreshContinueOnly();
    restoreFocusByKey(state.lastFocusedKey);
  };

  btnPlayPause.onclick = () => {
    if (videoEl.paused) videoEl.play().catch(() => {});
    else videoEl.pause();
    pokePlayerUi();
  };
  $('btn-restart').onclick = () => { cancelScrub(); seekToAbs(0); pokePlayerUi(); };
  videoEl.onclick = () => btnPlayPause.click();
  $('btn-rw').onclick = () => { cancelScrub(); seekToAbs(absCurrentTime() - 10); pokePlayerUi(); };
  $('btn-ff').onclick = () => { cancelScrub(); seekToAbs(absCurrentTime() + 30); pokePlayerUi(); };
  $('btn-next-ep').onclick = () => { if (state.nextEpisode) playMedia(state.nextEpisode, null, 0); };
  $('btn-upnext-play').onclick = () => { clearUpNext(); if (state.nextEpisode) playMedia(state.nextEpisode, null, 0); };
  $('btn-upnext-cancel').onclick = () => clearUpNext();

  $('btn-err-retry').onclick = () => { if (state.currentMedia) playMedia(state.currentMedia, QUALITIES[state.qualityIdx].value, absCurrentTime(), false, true); };
  $('btn-err-720').onclick = () => { if (state.currentMedia) playMedia(state.currentMedia, '720p', absCurrentTime(), false, true); };
  $('btn-err-cpu').onclick = () => { if (state.currentMedia) playMedia(state.currentMedia, 'cpu', absCurrentTime(), false, true); };
  $('btn-err-back').onclick = () => $('btn-close-player').click();

  $('btn-quality').onclick = () => {
    if (!state.currentMedia) return;
    state.qualityIdx = (state.qualityIdx + 1) % QUALITIES.length;
    playMedia(state.currentMedia, QUALITIES[state.qualityIdx].value, absCurrentTime(), false, true);
  };
  $('btn-sub').onclick = () => {
    openSubtitleModal();
  };
  $('btn-submodal-close').onclick = () => closeSubtitleModal();
  $('btn-sub-search-online').onclick = () => searchSubtitlesOnline();
  $('btn-sub-offset-minus').onclick = () => {
    state.subOffsetSecs = Math.round((state.subOffsetSecs - 0.5) * 10) / 10;
    $('sub-offset-label').textContent = `${state.subOffsetSecs >= 0 ? '+' : ''}${state.subOffsetSecs.toFixed(1)}s`;
    if (videoEl.textTracks[0]) applyCueOffset(videoEl.textTracks[0], -0.5);
  };
  $('btn-sub-offset-plus').onclick = () => {
    state.subOffsetSecs = Math.round((state.subOffsetSecs + 0.5) * 10) / 10;
    $('sub-offset-label').textContent = `${state.subOffsetSecs >= 0 ? '+' : ''}${state.subOffsetSecs.toFixed(1)}s`;
    if (videoEl.textTracks[0]) applyCueOffset(videoEl.textTracks[0], 0.5);
  };
  $('sub-modal')?.addEventListener('click', (e) => {
    if (e.target === $('sub-modal')) closeSubtitleModal();
  });

  // Seek bar mouse click + hover preview
  $('seek-wrap').onclick = (e) => {
    const rect = seekBar.getBoundingClientRect();
    const frac = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
    cancelScrub();
    seekToAbs(frac * totalDuration());
    pokePlayerUi();
  };
  $('seek-wrap').addEventListener('mousemove', (e) => {
    if (state.scrubTarget !== null) return;
    const rect = seekBar.getBoundingClientRect();
    const frac = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
    seekPreview.textContent = fmtClock(frac * totalDuration());
    seekPreview.style.left = `${Math.max(5, Math.min(95, frac * 100)).toFixed(2)}%`;
    seekPreview.classList.remove('hidden');
  });
  $('seek-wrap').addEventListener('mouseleave', () => {
    if (state.scrubTarget === null) seekPreview.classList.add('hidden');
  });

  $('btn-mute').onclick = () => {
    videoEl.muted = !videoEl.muted;
    $('btn-mute').textContent = videoEl.muted ? '🔇' : '🔊';
  };
  $('vol-slider').oninput = (e) => {
    videoEl.volume = Number(e.target.value);
    videoEl.muted = videoEl.volume === 0;
    $('btn-mute').textContent = videoEl.muted ? '🔇' : '🔊';
  };
  $('btn-fullscreen').onclick = () => {
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    else playerOverlay.requestFullscreen?.().catch(() => {});
  };
  playerOverlay.addEventListener('mousemove', pokePlayerUi);
  playerOverlay.addEventListener('click', (e) => {
    if (e.target === playerOverlay) {
      btnPlayPause.focus();
      btnPlayPause.click();
    }
  });
  document.querySelectorAll('.player-top, .player-bottom').forEach((bar) => {
    bar.addEventListener('mouseenter', () => {
      state.hudHovered = true;
      clearTimeout(hideUiTimer);
      playerOverlay.classList.remove('idle-hide');
    });
    bar.addEventListener('mouseleave', () => {
      state.hudHovered = false;
      pokePlayerUi();
    });
  });

  // Diagnostics & Navigation handlers
  $('btn-diag').onclick = async () => {
    const panel = $('diag-panel');
    if (panel.classList.contains('hidden')) {
      panel.classList.remove('hidden');
      await refreshDiagnostics();
    } else {
      const next = !state.diagEnabled;
      await api('/api/diagnostics/toggle', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled: next }),
      });
      await refreshDiagnostics();
      if (!next) panel.classList.add('hidden');
    }
  };
  $('btn-diag-refresh').onclick = () => refreshDiagnostics();
  $('btn-diag-hide').onclick = () => $('diag-panel').classList.add('hidden');

  document.querySelectorAll('.chip[data-sort]').forEach((b) => {
    b.onclick = () => { state.movieSort = b.dataset.sort; renderMovies(); b.focus(); syncUrl(true); };
  });
  document.querySelectorAll('.chip[data-show-sort]').forEach((b) => {
    b.onclick = () => { state.showSort = b.dataset.showSort; renderShows(); b.focus(); syncUrl(true); };
  });
  document.querySelectorAll('.nav-btn[data-tab]').forEach((b) => {
    b.onclick = () => {
      if (b.dataset.tab === 'shows') renderShows();
      if (b.dataset.tab === 'movies') renderMovies();
      if (b.dataset.tab === 'discover') loadDiscoverCatalog(false);
      switchTab(b.dataset.tab, true);
    };
  });
  $('btn-back-shows').onclick = () => {
    const prevShow = state.activeShow;
    renderShows();
    syncUrl(true);
    restoreFocusByKey(`show-${prevShow}`);
  };
  $('btn-scan').onclick = async () => {
    $('btn-scan').textContent = '↻ Scanning...';
    try { await api('/api/scan', { method: 'POST' }); await loadAll(); }
    finally { $('btn-scan').textContent = '↻ Scan'; }
  };

  async function runSearch(q, updateUrl = true) {
    if (state.searchAbort) state.searchAbort.abort();
    state.searchAbort = new AbortController();
    state.searchQuery = q;
    try {
      const data = await api(`/api/search?q=${encodeURIComponent(q)}`, { signal: state.searchAbort.signal });
      switchTab('search', false);
      if (updateUrl) syncUrl(false);
      const items = data.items || [];
      $('search-heading').textContent = `Search: "${q}" (${items.length})`;
      const g = $('search-grid');
      g.innerHTML = '';
      if (items.length === 0) {
        g.innerHTML = `<div class="empty-state">No matches found for "${escapeHtml(q)}".</div>`;
      } else {
        const frag = document.createDocumentFragment();
        items.forEach((m) => frag.appendChild(makeMediaCard(m)));
        g.appendChild(frag);
      }
    } catch (e) {
      if (e.name !== 'AbortError') console.error(e);
    }
  }

  let searchDebounce = null;
  $('search-input').oninput = (e) => {
    clearTimeout(searchDebounce);
    const q = e.target.value.trim();
    if (!q) { state.searchQuery = ''; switchTab('home', true); return; }
    searchDebounce = setTimeout(() => runSearch(q, true), 180);
  };

  window.addEventListener('hashchange', () => applyRouteFromUrl());

  // Unified Back stack (B button & Escape)
  function handleBack() {
    if (!playerOverlay.classList.contains('hidden')) {
      if (!$('sub-modal').classList.contains('hidden')) { closeSubtitleModal(); return; }
      if (state.scrubTarget !== null) { cancelScrub(); return; }
      if (!$('upnext-banner').classList.contains('hidden')) { clearUpNext(); return; }
      $('btn-close-player').click();
      return;
    }
    if (!$('discover-modal').classList.contains('hidden')) {
      closeDiscoverModal();
      return;
    }
    if (!$('diag-panel').classList.contains('hidden')) {
      $('diag-panel').classList.add('hidden');
      return;
    }
    if (document.activeElement === $('search-input')) {
      $('search-input').blur();
      if ($('search-input').value) { $('search-input').value = ''; state.searchQuery = ''; switchTab('home', true); }
      return;
    }
    if (!$('btn-back-shows').classList.contains('hidden')) {
      $('btn-back-shows').click();
      return;
    }
    if (state.tab !== 'home') {
      switchTab('home', true);
    }
  }

  // Improved spatial navigation using bounding-box overlap (handles wide seek-bar cleanly!)
  function visibleFocusables() {
    let scope = document;
    if (!playerOverlay.classList.contains('hidden')) {
      if (!$('sub-modal').classList.contains('hidden')) scope = $('sub-modal');
      else if (!$('player-error-banner').classList.contains('hidden')) scope = $('player-error-banner');
      else if (!$('upnext-banner').classList.contains('hidden')) scope = $('upnext-banner');
      else scope = playerOverlay;
    } else if (!$('discover-modal').classList.contains('hidden')) {
      scope = $('discover-modal');
    }
    return Array.from(scope.querySelectorAll('.focusable')).filter((el) => el.offsetParent !== null);
  }

  function intervalGap(a1, a2, b1, b2) {
    if (a2 < b1) return b1 - a2;
    if (b2 < a1) return a1 - b2;
    return 0;
  }

  function moveFocus(dir) {
    const items = visibleFocusables();
    if (!items.length) return;
    const cur = document.activeElement;
    if (!items.includes(cur)) { items[0].focus(); return; }
    const r = cur.getBoundingClientRect();
    const rCx = r.left + r.width / 2;
    const rCy = r.top + r.height / 2;
    let best = null;
    let bestScore = Infinity;

    if (dir === 'right' || dir === 'left') {
      // Pass 1: Strict same-row navigation (stay on the current row until start/end)
      for (const el of items) {
        if (el === cur) continue;
        const er = el.getBoundingClientRect();
        if (intervalGap(r.top, r.bottom, er.top, er.bottom) > 8) continue;
        const erCx = er.left + er.width / 2;
        if (dir === 'right' && erCx <= rCx + 4) continue;
        if (dir === 'left' && erCx >= rCx - 4) continue;
        const primary = dir === 'right' ? Math.max(0, er.left - r.right) : Math.max(0, r.left - er.right);
        const score = primary + Math.abs(erCx - rCx) * 0.1 + Math.abs((er.top + er.height / 2) - rCy) * 0.2;
        if (score < bestScore) { bestScore = score; best = el; }
      }

      // Pass 2: At row start/end, wrap cleanly to the adjacent row
      if (!best) {
        const adj = items.filter((el) => {
          if (el === cur) return false;
          const er = el.getBoundingClientRect();
          return dir === 'right' ? er.top >= r.bottom - 6 : er.bottom <= r.top + 6;
        });
        if (adj.length > 0) {
          const minRowDist = Math.min(
            ...adj.map((el) => {
              const er = el.getBoundingClientRect();
              return dir === 'right' ? Math.max(0, er.top - r.bottom) : Math.max(0, r.top - er.bottom);
            })
          );
          const nextRow = adj.filter((el) => {
            const er = el.getBoundingClientRect();
            const d = dir === 'right' ? Math.max(0, er.top - r.bottom) : Math.max(0, r.top - er.bottom);
            return d <= minRowDist + 24;
          });
          nextRow.sort((a, b) => {
            const ar = a.getBoundingClientRect();
            const br = b.getBoundingClientRect();
            return dir === 'right' ? ar.left - br.left : br.right - ar.right;
          });
          best = nextRow[0] || null;
        }
      }
    } else {
      // Up / Down: strictly jump to another row, closest row vertically then closest horizontally
      for (const el of items) {
        if (el === cur) continue;
        const er = el.getBoundingClientRect();
        if (dir === 'down' && er.top < r.bottom - 6) continue;
        if (dir === 'up' && er.bottom > r.top + 6) continue;
        const primary = dir === 'down' ? Math.max(0, er.top - r.bottom) : Math.max(0, r.top - er.bottom);
        const hGap = intervalGap(r.left, r.right, er.left, er.right);
        const cxDiff = Math.abs((er.left + er.width / 2) - rCx);
        const score = primary * 2.5 + hGap * 2.0 + cxDiff * 0.35;
        if (score < bestScore) { bestScore = score; best = el; }
      }
    }

    if (best) {
      best.focus();
      best.scrollIntoView({ block: 'nearest', behavior: 'auto' });
    }
  }

  window.addEventListener('keydown', (e) => {
    if (document.activeElement === $('search-input')) {
      if (e.key === 'Escape') { e.preventDefault(); handleBack(); }
      return;
    }
    const inPlayer = !playerOverlay.classList.contains('hidden') && $('sub-modal').classList.contains('hidden');
    if (inPlayer) {
      const wasIdle = playerOverlay.classList.contains('idle-hide');
      const looseFocus = !document.activeElement || document.activeElement === document.body || document.activeElement === videoEl || !playerOverlay.contains(document.activeElement);
      pokePlayerUi();
      if (e.key === 'MediaPlayPause' || e.key === ' ' || e.key === 'k' || e.key === 'K') {
        e.preventDefault();
        btnPlayPause.focus();
        btnPlayPause.click();
        return;
      }
      if (e.key === 'MediaPlay') { e.preventDefault(); videoEl.play().catch(() => {}); pokePlayerUi(); return; }
      if (e.key === 'MediaPause') { e.preventDefault(); videoEl.pause(); pokePlayerUi(); return; }
      if (e.key === 'MediaStop') { e.preventDefault(); $('btn-close-player').click(); return; }
      if (e.key === 'Enter') {
        if (state.scrubTarget !== null) { e.preventDefault(); commitScrub(); return; }
        if (wasIdle || looseFocus || document.activeElement === seekBar) {
          e.preventDefault();
          btnPlayPause.focus();
          btnPlayPause.click();
          return;
        }
      }
      if ((wasIdle || looseFocus) && (e.key === 'ArrowDown' || e.key === 'ArrowUp')) {
        e.preventDefault();
        cancelScrub();
        btnPlayPause.focus();
        return;
      }
      const onSeekBar = document.activeElement === seekBar || wasIdle || looseFocus;
      if (e.key === 'ArrowRight' && onSeekBar) { e.preventDefault(); seekBar.focus(); stepScrub(1, false); return; }
      if (e.key === 'ArrowLeft' && onSeekBar) { e.preventDefault(); seekBar.focus(); stepScrub(-1, false); return; }
      if (e.key === 'ArrowDown' && document.activeElement === seekBar) { e.preventDefault(); cancelScrub(); btnPlayPause.focus(); return; }
      if (e.key === '0' || e.key === 'Home') { e.preventDefault(); $('btn-restart').click(); return; }
      if (e.key === 'j' || e.key === 'J') { e.preventDefault(); $('btn-rw').click(); return; }
      if (e.key === 'l' || e.key === 'L') { e.preventDefault(); $('btn-ff').click(); return; }
      if (e.key === 'm' || e.key === 'M') { e.preventDefault(); $('btn-mute').click(); return; }
      if (e.key === 'f' || e.key === 'F') { e.preventDefault(); $('btn-fullscreen').click(); return; }
      if (e.key === 'ArrowUp' || e.key === 'ArrowDown') cancelScrub();
    }
    if (e.key === 'Escape' || e.key === 'Backspace') { e.preventDefault(); handleBack(); return; }
    if (e.key === 'ArrowRight') { e.preventDefault(); moveFocus('right'); }
    else if (e.key === 'ArrowLeft') { e.preventDefault(); moveFocus('left'); }
    else if (e.key === 'ArrowDown') { e.preventDefault(); moveFocus('down'); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); moveFocus('up'); }
  });

  window.addEventListener('keyup', (e) => {
    if (e.key === 'ArrowLeft' || e.key === 'ArrowRight') {
      state.scrubHoldCount = 0;
    }
  });

  // Event-driven Xbox Gamepad polling (0% CPU when no controller connected!)
  function activeGamepad() {
    if (!navigator.getGamepads) return null;
    return Array.from(navigator.getGamepads()).find((p) => p && p.connected) || null;
  }

  function pollGamepad() {
    if (document.hidden) { state.gamepadRaf = null; return; }
    const pad = activeGamepad();
    if (!pad) { state.gamepadRaf = null; state.prevPadButtons = []; return; }

    const now = performance.now();
    const pressed = (i) => !!pad.buttons[i]?.pressed;
    const justPressed = (i) => pressed(i) && !state.prevPadButtons[i];

    const inPlayer = !playerOverlay.classList.contains('hidden') && $('sub-modal').classList.contains('hidden');
    const axX = pad.axes[0] || 0;
    const axY = pad.axes[1] || 0;
    const horizDominant = Math.abs(axX) >= Math.abs(axY);
    const rightHeld = pressed(15) || (horizDominant && axX > 0.55);
    const leftHeld = pressed(14) || (horizDominant && axX < -0.55);
    const downHeld = pressed(13) || (!horizDominant && axY > 0.55);
    const upHeld = pressed(12) || (!horizDominant && axY < -0.55);

    if (!leftHeld && !rightHeld) state.scrubHoldCount = 0;

    // Edge-triggered action buttons (never double-fire when held!)
    if (inPlayer) {
      if (justPressed(4)) { cancelScrub(); seekToAbs(absCurrentTime() - 10); pokePlayerUi(); }
      else if (justPressed(5)) { cancelScrub(); seekToAbs(absCurrentTime() + 30); pokePlayerUi(); }
      else if (justPressed(6)) { cancelScrub(); seekToAbs(absCurrentTime() - 60); pokePlayerUi(); }
      else if (justPressed(7)) { cancelScrub(); seekToAbs(absCurrentTime() + 60); pokePlayerUi(); }
      else if (justPressed(2)) { btnPlayPause.click(); }
      else if (justPressed(0)) {
        const looseFocus = !document.activeElement || document.activeElement === document.body || document.activeElement === videoEl || !playerOverlay.contains(document.activeElement);
        if (state.scrubTarget !== null) commitScrub();
        else if (playerOverlay.classList.contains('idle-hide') || looseFocus || document.activeElement === seekBar) {
          pokePlayerUi();
          btnPlayPause.focus();
          btnPlayPause.click();
        } else {
          pokePlayerUi();
          document.activeElement?.click();
        }
      } else if (justPressed(1)) {
        handleBack();
      }
    } else {
      if (justPressed(0)) document.activeElement?.click();
      else if (justPressed(1)) handleBack();
    }

    // Smooth repeatable D-pad / Stick navigation & scrubbing
    if (now - state.lastGamepadNav > 150) {
      if (inPlayer) {
        const wasIdle = playerOverlay.classList.contains('idle-hide');
        const looseFocus = !document.activeElement || document.activeElement === document.body || document.activeElement === videoEl || !playerOverlay.contains(document.activeElement);
        const onSeekBar = document.activeElement === seekBar || wasIdle || looseFocus;
        if ((wasIdle || looseFocus) && (downHeld || upHeld)) {
          cancelScrub();
          pokePlayerUi();
          btnPlayPause.focus();
          state.lastGamepadNav = now;
        }
        else if (rightHeld && onSeekBar) { seekBar.focus(); stepScrub(1, true); state.lastGamepadNav = now; }
        else if (leftHeld && onSeekBar) { seekBar.focus(); stepScrub(1 * -1, true); state.lastGamepadNav = now; }
        else if (downHeld && document.activeElement === seekBar) { cancelScrub(); pokePlayerUi(); btnPlayPause.focus(); state.lastGamepadNav = now; }
        else if (rightHeld) { pokePlayerUi(); moveFocus('right'); state.lastGamepadNav = now; }
        else if (leftHeld) { pokePlayerUi(); moveFocus('left'); state.lastGamepadNav = now; }
        else if (downHeld) { cancelScrub(); pokePlayerUi(); moveFocus('down'); state.lastGamepadNav = now; }
        else if (upHeld) { cancelScrub(); pokePlayerUi(); moveFocus('up'); state.lastGamepadNav = now; }
      } else {
        if (rightHeld) { moveFocus('right'); state.lastGamepadNav = now; }
        else if (leftHeld) { moveFocus('left'); state.lastGamepadNav = now; }
        else if (downHeld) { moveFocus('down'); state.lastGamepadNav = now; }
        else if (upHeld) { moveFocus('up'); state.lastGamepadNav = now; }
      }
    }

    state.prevPadButtons = pad.buttons.map((b) => !!b.pressed);
    state.gamepadRaf = requestAnimationFrame(pollGamepad);
  }

  function startGamepadLoop() {
    if (!state.gamepadRaf && !document.hidden && activeGamepad()) {
      state.gamepadRaf = requestAnimationFrame(pollGamepad);
    }
  }

  window.addEventListener('gamepadconnected', startGamepadLoop);
  window.addEventListener('gamepaddisconnected', () => {
    if (!activeGamepad() && state.gamepadRaf) {
      cancelAnimationFrame(state.gamepadRaf);
      state.gamepadRaf = null;
      state.prevPadButtons = [];
    }
  });
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) startGamepadLoop();
  });
  // Xbox Edge fallback: gamepadconnected does not always fire until after input
  setInterval(() => {
    if (!state.gamepadRaf && !document.hidden && activeGamepad()) {
      startGamepadLoop();
    }
  }, 1000);

  loadAll();
  startGamepadLoop();
})();
