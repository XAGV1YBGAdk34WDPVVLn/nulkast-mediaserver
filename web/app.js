(() => {
  const state = {
    tab: 'home',
    movies: [],
    movieSort: 'added',
    movieGenre: 'All',
    shows: [],
    showSort: 'added',
    showGenre: 'All',
    activeShow: null,
    searchQuery: '',
    continueList: [],
    currentMedia: null,
    currentSessionId: null,
    currentMode: '',
    startOffset: 0,
    hls: null,
    retryCount: 0,
    scrubTarget: null,
    scrubHoldCount: 0,
    diagEnabled: true,
    progressTimer: null,
    statsTimer: null,
    lastGamepadNav: 0,
  };

  const $ = (id) => document.getElementById(id);
  const videoEl = $('video-el');

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

  function totalDuration() {
    const metaDur = state.currentMedia?.duration_secs || 0;
    const vidDur = isFinite(videoEl.duration) ? videoEl.duration : 0;
    return Math.max(metaDur, (state.currentMode === 'direct' ? 0 : state.startOffset) + vidDur, 1);
  }

  function absCurrentTime() {
    return (state.currentMode === 'direct' ? 0 : state.startOffset) + (videoEl.currentTime || 0);
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
    // Fast client-side seek if target is already transcoded in this HLS session
    if (relTarget >= 0 && relTarget <= Math.max(bufEnd, hlsDur) - 1) {
      videoEl.currentTime = relTarget;
    } else {
      // Instant server-side -ss seek into /dev/shm
      playMedia(state.currentMedia, $('quality-select').value, clamped);
    }
  }

  let hideUiTimer = null;
  function pokePlayerUi() {
    const ov = $('player-overlay');
    if (ov.classList.contains('hidden')) return;
    ov.classList.remove('idle-hide');
    clearTimeout(hideUiTimer);
    if (!videoEl.paused && state.scrubTarget === null) {
      hideUiTimer = setTimeout(() => ov.classList.add('idle-hide'), 4200);
    }
  }

  function stepScrub(dir) {
    if (!state.currentMedia) return;
    pokePlayerUi();
    const total = totalDuration();
    if (state.scrubTarget === null) {
      state.scrubTarget = absCurrentTime();
      state.scrubHoldCount = 0;
    }
    state.scrubHoldCount += 1;
    // Accelerate scrub step while holding stick/D-pad: 10s -> 30s -> 60s
    const step = state.scrubHoldCount > 12 ? 60 : state.scrubHoldCount > 5 ? 30 : 10;
    state.scrubTarget = Math.max(0, Math.min(total - 1, state.scrubTarget + dir * step));
    const pct = Math.min(100, (state.scrubTarget / total) * 100);
    $('seek-played').style.width = `${pct.toFixed(2)}%`;
    $('seek-thumb').style.left = `${pct.toFixed(2)}%`;
    const prev = $('seek-preview');
    prev.textContent = `${fmtClock(state.scrubTarget)} (Press A to jump)`;
    prev.style.left = `${Math.max(8, Math.min(92, pct)).toFixed(2)}%`;
    prev.classList.remove('hidden');
    $('player-time').textContent = `${fmtClock(state.scrubTarget)} / ${fmtClock(total)}`;
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
    $('seek-preview')?.classList.add('hidden');
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

  function escapeHtml(s) {
    return String(s || '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  }

  function makeMediaCard(item) {
    const btn = document.createElement('button');
    btn.className = 'card focusable';
    const pct = item.duration_secs > 10 ? Math.min(100, (item.position_secs / item.duration_secs) * 100) : 0;
    const resTag = item.height >= 2000 ? '4K' : item.height >= 1000 ? '1080p' : item.height > 0 ? `${item.height}p` : '';
    const subTitle = item.show_name ? `${item.show_name} • ${item.title}` : item.title;

    const posterKey = encodeURIComponent(`${item.title}-${item.year || ''}`);
    btn.innerHTML = `
      <div class="card-thumb-wrap poster-ratio">
        <img class="card-thumb" loading="lazy" src="/api/media/${item.id}/poster?k=${posterKey}" alt="" onerror="this.style.display='none'" />
      </div>
      <div class="card-body">
        <div class="card-title">${escapeHtml(subTitle)}</div>
        <div class="card-meta">
          ${item.year ? `<span class="tag">${item.year}</span>` : ''}
          ${resTag ? `<span class="tag">${resTag}</span>` : ''}
          ${item.video_codec ? `<span class="tag">${escapeHtml(item.video_codec)}</span>` : ''}
          ${item.audio_codec ? `<span class="tag">${escapeHtml(item.audio_codec)}</span>` : ''}
          ${item.duration_secs ? `<span>${fmtDuration(item.duration_secs)}</span>` : ''}
        </div>
        ${pct > 1 ? `<div class="progress-bar"><div class="progress-fill" style="width:${pct.toFixed(1)}%"></div></div>` : ''}
      </div>
    `;
    btn.onclick = () => playMedia(item);
    return btn;
  }

  function makeShowCard(show) {
    const btn = document.createElement('button');
    btn.className = 'card focusable';
    btn.innerHTML = `
      <div class="card-thumb-wrap poster-ratio">
        <img class="card-thumb" loading="lazy" src="/api/shows/${encodeURIComponent(show.show_name)}/poster" alt="" onerror="this.onerror=null;this.src='/api/media/${show.latest_id}/thumb'" />
      </div>
      <div class="card-body">
        <div class="card-title">${escapeHtml(show.show_name)}</div>
        <div class="card-meta">
          <span class="tag">${show.season_count} ${show.season_count === 1 ? 'Season' : 'Seasons'}</span>
          <span class="tag">${show.episode_count} Eps</span>
        </div>
      </div>
    `;
    btn.onclick = () => openShow(show.show_name);
    return btn;
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
    } else if (route === 'search') {
      const q = params.get('q') || '';
      state.searchQuery = q;
      $('search-input').value = q;
      if (q) {
        await runSearch(q, false);
      } else {
        switchTab('home', false);
      }
    } else {
      state.activeShow = null;
      switchTab('home', false);
    }
  }

  async function loadAll() {
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
      $('btn-diag').textContent = state.diagEnabled ? 'Diag: ON' : 'Diag: OFF';
      $('hw-badge').textContent = health.hw_vaapi ? 'INTEL VA-API + RAM HLS' : 'CPU + RAM HLS';
      state.movies = movies.items || [];
      state.shows = shows.shows || [];
      state.continueList = cont.items || [];
      renderHome();
      renderMovies();
      renderShows();
      await applyRouteFromUrl();
    } catch (e) {
      $('hw-badge').textContent = 'OFFLINE';
      console.error(e);
    }
  }

  function renderHome() {
    const cSec = $('continue-section');
    const cGrid = $('continue-grid');
    cGrid.innerHTML = '';
    if (state.continueList.length > 0) {
      cSec.classList.remove('hidden');
      state.continueList.forEach((m) => cGrid.appendChild(makeMediaCard(m)));
    } else {
      cSec.classList.add('hidden');
    }

    const mGrid = $('home-movies-grid');
    mGrid.innerHTML = '';
    const newestMovies = state.movies.slice().sort((a, b) => (b.mtime || 0) - (a.mtime || 0));
    newestMovies.slice(0, 18).forEach((m) => mGrid.appendChild(makeMediaCard(m)));

    const sGrid = $('home-shows-grid');
    sGrid.innerHTML = '';
    state.shows.slice(0, 18).forEach((s) => sGrid.appendChild(makeShowCard(s)));
  }

  function renderMovies() {
    // Build dynamic genre chips with counts from all movies
    const counts = new Map();
    state.movies.forEach((m) => {
      (m.genres || '').split(',').map((g) => g.trim()).filter(Boolean).forEach((g) => {
        counts.set(g, (counts.get(g) || 0) + 1);
      });
    });

    const genreRow = $('genre-chips');
    if (genreRow) {
      genreRow.innerHTML = '<span class="chip-label">Category:</span>';
      const allBtn = document.createElement('button');
      allBtn.className = `chip focusable${state.movieGenre === 'All' ? ' active' : ''}`;
      allBtn.textContent = `All (${state.movies.length})`;
      allBtn.onclick = () => { state.movieGenre = 'All'; renderMovies(); syncUrl(true); };
      genreRow.appendChild(allBtn);

      Array.from(counts.entries())
        .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
        .forEach(([genre, cnt]) => {
          const b = document.createElement('button');
          b.className = `chip focusable${state.movieGenre === genre ? ' active' : ''}`;
          b.textContent = `${genre} (${cnt})`;
          b.onclick = () => { state.movieGenre = genre; renderMovies(); syncUrl(true); };
          genreRow.appendChild(b);
        });
    }

    // Highlight active sort chip
    document.querySelectorAll('.chip[data-sort]').forEach((b) => {
      b.classList.toggle('active', b.dataset.sort === state.movieSort);
    });

    // Filter + sort movies locally (instant 0ms response on Xbox controller!)
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

    const heading = $('movies-heading');
    if (heading) {
      heading.textContent = state.movieGenre === 'All'
        ? `All Movies (${list.length})`
        : `${state.movieGenre} Movies (${list.length})`;
    }

    const g = $('movies-grid');
    g.innerHTML = '';
    list.forEach((m) => g.appendChild(makeMediaCard(m)));
  }

  function renderShows() {
    state.activeShow = null;
    $('btn-back-shows').classList.add('hidden');
    $('episodes-list').classList.add('hidden');
    if ($('shows-toolbar')) $('shows-toolbar').classList.remove('hidden');

    // Build dynamic TV show genre chips
    const counts = new Map();
    state.shows.forEach((s) => {
      (s.genres || '').split(',').map((g) => g.trim()).filter(Boolean).forEach((g) => {
        counts.set(g, (counts.get(g) || 0) + 1);
      });
    });

    const genreRow = $('show-genre-chips');
    if (genreRow) {
      genreRow.innerHTML = '<span class="chip-label">Category:</span>';
      const allBtn = document.createElement('button');
      allBtn.className = `chip focusable${state.showGenre === 'All' ? ' active' : ''}`;
      allBtn.textContent = `All (${state.shows.length})`;
      allBtn.onclick = () => { state.showGenre = 'All'; renderShows(); syncUrl(true); };
      genreRow.appendChild(allBtn);

      Array.from(counts.entries())
        .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
        .forEach(([genre, cnt]) => {
          const b = document.createElement('button');
          b.className = `chip focusable${state.showGenre === genre ? ' active' : ''}`;
          b.textContent = `${genre} (${cnt})`;
          b.onclick = () => { state.showGenre = genre; renderShows(); syncUrl(true); };
          genreRow.appendChild(b);
        });
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
    list.forEach((s) => g.appendChild(makeShowCard(s)));
  }

  async function openShow(showName, updateUrl = true) {
    state.activeShow = showName;
    switchTab('shows', false);
    if (updateUrl) syncUrl(true);
    $('shows-heading').textContent = showName;
    $('btn-back-shows').classList.remove('hidden');
    if ($('shows-toolbar')) $('shows-toolbar').classList.add('hidden');
    $('shows-grid').classList.add('hidden');
    const list = $('episodes-list');
    list.classList.remove('hidden');
    list.innerHTML = '<div style="color:var(--muted)">Loading episodes...</div>';

    const data = await api(`/api/shows/${encodeURIComponent(showName)}/episodes`);
    list.innerHTML = '';
    (data.episodes || []).forEach((ep) => {
      const row = document.createElement('button');
      row.className = 'episode-row focusable';
      const pct = ep.duration_secs > 10 ? Math.min(100, (ep.position_secs / ep.duration_secs) * 100) : 0;
      row.innerHTML = `
        <div class="ep-left">
          <img class="ep-thumb" loading="lazy" src="/api/media/${ep.id}/thumb" alt="" onerror="this.style.display='none'" />
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
      row.onclick = () => playMedia(ep);
      list.appendChild(row);
    });
  }

  function switchTab(tab, updateUrl = true) {
    state.tab = tab;
    document.querySelectorAll('.nav-btn[data-tab]').forEach((b) => {
      b.classList.toggle('active', b.dataset.tab === tab);
    });
    document.querySelectorAll('.view').forEach((v) => v.classList.remove('active'));
    const el = $(`view-${tab}`);
    if (el) el.classList.add('active');
    if (updateUrl) syncUrl(true);
  }

  async function stopActiveSession() {
    if (state.progressTimer) { clearInterval(state.progressTimer); state.progressTimer = null; }
    if (state.statsTimer) { clearInterval(state.statsTimer); state.statsTimer = null; }
    if (state.hls) { state.hls.destroy(); state.hls = null; }
    videoEl.pause();
    videoEl.removeAttribute('src');
    videoEl.querySelectorAll('track').forEach((t) => t.remove());
    videoEl.load();
    if (state.currentSessionId) {
      const sid = state.currentSessionId;
      state.currentSessionId = null;
      fetch(`/api/hls/${sid}`, { method: 'DELETE' }).catch(() => {});
    }
  }

  async function playMedia(item, qualityOverride = null, startOverride = null, isRetry = false) {
    await stopActiveSession();
    if (!isRetry) state.retryCount = 0;
    state.currentMedia = item;
    const quality = qualityOverride || $('quality-select').value || 'auto';
    const startSecs = startOverride !== null ? startOverride : (item.position_secs > 10 && !item.watched ? item.position_secs : 0);
    state.startOffset = startSecs;

    cancelScrub();
    document.documentElement.classList.add('player-open');
    document.body.classList.add('player-open');
    $('player-overlay').classList.remove('hidden');
    $('player-title').textContent = item.show_name ? `${item.show_name} — ${item.title}` : item.title;
    $('player-stats').textContent = 'Starting stream in RAM...';
    pokePlayerUi();
    setTimeout(() => $('seek-bar')?.focus(), 60);

    const subSel = $('sub-select');
    subSel.innerHTML = '<option value="-1">Subtitles: Off</option>';
    try {
      const subs = JSON.parse(item.subtitles_json || '[]');
      subs.filter((s) => s.is_text).forEach((s) => {
        const opt = document.createElement('option');
        opt.value = String(s.index);
        opt.textContent = `${s.title} (${s.language})`;
        subSel.appendChild(opt);
      });
    } catch {}

    let resp;
    try {
      resp = await api(`/api/stream/${item.id}/start`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ start_secs: startSecs, quality }),
      });
    } catch (e) {
      $('player-stats').textContent = `Stream start failed: ${e.message}`;
      clientLog('error', `Start failed media=${item.id}: ${e.message}`);
      return;
    }

    state.currentSessionId = resp.session_id;
    state.currentMode = resp.mode;
    clientLog('info', `Playing media=${item.id} mode=${resp.mode} hw=${resp.hw_accel} url=${resp.stream_url}`);

    if (resp.mode === 'direct') {
      videoEl.src = resp.stream_url;
      if (startSecs > 0) {
        videoEl.addEventListener('loadedmetadata', () => { videoEl.currentTime = startSecs; }, { once: true });
      }
      videoEl.play().catch((err) => clientLog('warn', `Autoplay blocked: ${err.message}`));
    } else if (window.Hls && window.Hls.isSupported()) {
      const hls = new window.Hls({
        maxBufferLength: 90,
        maxMaxBufferLength: 300,
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
            const pos = (state.currentMode === 'direct' ? 0 : state.startOffset) + (videoEl.currentTime || 0);
            playMedia(state.currentMedia, $('quality-select').value, pos, true);
          } else {
            $('player-stats').textContent = `Playback error: ${data.details || data.type}`;
          }
        }
      });
    } else {
      videoEl.src = resp.stream_url;
      videoEl.play().catch(() => {});
    }

    state.statsTimer = setInterval(() => {
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

      $('seek-buffered').style.left = `${bufStartPct.toFixed(2)}%`;
      $('seek-buffered').style.width = `${bufWidthPct.toFixed(2)}%`;
      if (state.scrubTarget === null) {
        $('seek-played').style.width = `${playedPct.toFixed(2)}%`;
        $('seek-thumb').style.left = `${playedPct.toFixed(2)}%`;
        $('player-time').textContent = `${fmtClock(absCur)} / ${fmtClock(total)}`;
      }
      $('btn-play-pause').textContent = videoEl.paused ? '▶ Play' : '⏸ Pause';

      const modeLabel = resp.mode === 'direct' ? 'DIRECT PLAY' : resp.mode === 'remux' ? 'ZERO-COPY REMUX' : resp.hw_accel ? 'INTEL VA-API HW' : 'CPU HLS';
      const diagExtra = state.diagEnabled
        ? ` • ${videoEl.videoWidth || 0}x${videoEl.videoHeight || 0} • ready=${videoEl.readyState}`
        : '';
      $('player-stats').textContent = `${modeLabel} • Buffer: ${bufAhead.toFixed(0)}s ahead${diagExtra}`;
    }, 350);

    state.progressTimer = setInterval(() => {
      if (!state.currentMedia || videoEl.paused) return;
      const absPos = (state.currentMode === 'direct' ? 0 : state.startOffset) + (videoEl.currentTime || 0);
      fetch(`/api/media/${state.currentMedia.id}/progress`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ position_secs: absPos }),
      }).catch(() => {});
    }, 5000);
  }

  $('btn-close-player').onclick = async () => {
    cancelScrub();
    await stopActiveSession();
    document.documentElement.classList.remove('player-open');
    document.body.classList.remove('player-open');
    $('player-overlay').classList.add('hidden');
    loadAll();
  };

  $('btn-play-pause').onclick = () => {
    if (videoEl.paused) videoEl.play().catch(() => {});
    else videoEl.pause();
    pokePlayerUi();
  };
  $('btn-restart').onclick = () => {
    cancelScrub();
    seekToAbs(0);
    pokePlayerUi();
  };
  videoEl.onclick = () => $('btn-play-pause').click();
  $('btn-rw').onclick = () => { cancelScrub(); seekToAbs(absCurrentTime() - 10); pokePlayerUi(); };
  $('btn-ff').onclick = () => { cancelScrub(); seekToAbs(absCurrentTime() + 30); pokePlayerUi(); };
  $('seek-bar').onclick = (e) => {
    const rect = $('seek-bar').getBoundingClientRect();
    const frac = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
    seekToAbs(frac * totalDuration());
    pokePlayerUi();
  };
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
    const ov = $('player-overlay');
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    else ov.requestFullscreen?.().catch(() => {});
  };
  $('player-overlay').addEventListener('mousemove', pokePlayerUi);
  $('player-overlay').addEventListener('keydown', pokePlayerUi);

  $('quality-select').onchange = () => {
    if (!state.currentMedia) return;
    const pos = (state.currentMode === 'direct' ? 0 : state.startOffset) + (videoEl.currentTime || 0);
    playMedia(state.currentMedia, $('quality-select').value, pos);
  };

  $('sub-select').onchange = () => {
    videoEl.querySelectorAll('track').forEach((t) => t.remove());
    const idx = Number($('sub-select').value);
    if (idx < 0 || !state.currentMedia) return;
    const track = document.createElement('track');
    track.kind = 'subtitles';
    track.label = 'Subtitles';
    track.src = `/api/stream/${state.currentMedia.id}/subtitles/${idx}`;
    track.default = true;
    videoEl.appendChild(track);
    if (videoEl.textTracks[0]) videoEl.textTracks[0].mode = 'showing';
  };

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
    b.onclick = () => {
      state.movieSort = b.dataset.sort;
      renderMovies();
      syncUrl(true);
    };
  });

  document.querySelectorAll('.chip[data-show-sort]').forEach((b) => {
    b.onclick = () => {
      state.showSort = b.dataset.showSort;
      renderShows();
      syncUrl(true);
    };
  });

  document.querySelectorAll('.nav-btn[data-tab]').forEach((b) => {
    b.onclick = () => {
      if (b.dataset.tab === 'shows') renderShows();
      if (b.dataset.tab === 'movies') renderMovies();
      switchTab(b.dataset.tab, true);
    };
  });

  $('btn-back-shows').onclick = () => { renderShows(); syncUrl(true); };

  $('btn-scan').onclick = async () => {
    $('btn-scan').textContent = '↻ Scanning...';
    try {
      await api('/api/scan', { method: 'POST' });
      await loadAll();
    } finally {
      $('btn-scan').textContent = '↻ Scan';
    }
  };

  async function runSearch(q, updateUrl = true) {
    state.searchQuery = q;
    const data = await api(`/api/search?q=${encodeURIComponent(q)}`);
    switchTab('search', false);
    if (updateUrl) syncUrl(false);
    $('search-heading').textContent = `Search: "${q}" (${(data.items || []).length})`;
    const g = $('search-grid');
    g.innerHTML = '';
    (data.items || []).forEach((m) => g.appendChild(makeMediaCard(m)));
  }

  let searchDebounce = null;
  $('search-input').oninput = (e) => {
    clearTimeout(searchDebounce);
    const q = e.target.value.trim();
    if (!q) { state.searchQuery = ''; switchTab('home', true); return; }
    searchDebounce = setTimeout(() => runSearch(q, true), 180);
  };

  window.addEventListener('hashchange', () => {
    applyRouteFromUrl();
  });

  function visibleFocusables() {
    const scope = !$('player-overlay').classList.contains('hidden') ? $('player-overlay') : document;
    return Array.from(scope.querySelectorAll('.focusable')).filter((el) => el.offsetParent !== null);
  }

  function moveFocus(dir) {
    const items = visibleFocusables();
    if (!items.length) return;
    const cur = document.activeElement;
    if (!items.includes(cur)) { items[0].focus(); return; }
    const r = cur.getBoundingClientRect();
    const cx = r.left + r.width / 2;
    const cy = r.top + r.height / 2;
    let best = null;
    let bestScore = Infinity;

    for (const el of items) {
      if (el === cur) continue;
      const er = el.getBoundingClientRect();
      const dx = (er.left + er.width / 2) - cx;
      const dy = (er.top + er.height / 2) - cy;
      if (dir === 'right' && dx <= 10) continue;
      if (dir === 'left' && dx >= -10) continue;
      if (dir === 'down' && dy <= 10) continue;
      if (dir === 'up' && dy >= -10) continue;
      const primary = (dir === 'left' || dir === 'right') ? Math.abs(dx) : Math.abs(dy);
      const secondary = (dir === 'left' || dir === 'right') ? Math.abs(dy) : Math.abs(dx);
      const score = primary + secondary * 2.5;
      if (score < bestScore) { bestScore = score; best = el; }
    }
    if (best) {
      best.focus();
      best.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    }
  }

  window.addEventListener('keydown', (e) => {
    if (document.activeElement === $('search-input') && e.key !== 'Escape') return;
    const inPlayer = !$('player-overlay').classList.contains('hidden');
    if (inPlayer) {
      pokePlayerUi();
      const onSeekBar = document.activeElement === $('seek-bar') || $('player-overlay').classList.contains('idle-hide');
      if (e.key === 'ArrowRight' && onSeekBar) { e.preventDefault(); stepScrub(1); return; }
      if (e.key === 'ArrowLeft' && onSeekBar) { e.preventDefault(); stepScrub(-1); return; }
      if (e.key === 'Enter' && state.scrubTarget !== null) { e.preventDefault(); commitScrub(); return; }
      if (e.key === ' ' || e.key === 'k' || e.key === 'K') { e.preventDefault(); $('btn-play-pause').click(); return; }
      if (e.key === '0' || e.key === 'Home') { e.preventDefault(); $('btn-restart').click(); return; }
      if (e.key === 'j' || e.key === 'J') { e.preventDefault(); $('btn-rw').click(); return; }
      if (e.key === 'l' || e.key === 'L') { e.preventDefault(); $('btn-ff').click(); return; }
      if (e.key === 'm' || e.key === 'M') { e.preventDefault(); $('btn-mute').click(); return; }
      if (e.key === 'f' || e.key === 'F') { e.preventDefault(); $('btn-fullscreen').click(); return; }
      if (e.key === 'Escape' || e.key === 'Backspace') {
        e.preventDefault();
        if (state.scrubTarget !== null) cancelScrub();
        else $('btn-close-player').click();
        return;
      }
      if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
        cancelScrub();
      }
    }
    if (e.key === 'ArrowRight') { e.preventDefault(); moveFocus('right'); }
    else if (e.key === 'ArrowLeft') { e.preventDefault(); moveFocus('left'); }
    else if (e.key === 'ArrowDown') { e.preventDefault(); moveFocus('down'); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); moveFocus('up'); }
  });

  function pollGamepad() {
    const now = performance.now();
    const pads = navigator.getGamepads ? navigator.getGamepads() : [];
    const pad = pads && pads[0];
    if (pad) {
      const inPlayer = !$('player-overlay').classList.contains('hidden');
      const axX = pad.axes[0] || 0;
      const axY = pad.axes[1] || 0;
      const rightHeld = pad.buttons[15]?.pressed || axX > 0.55;
      const leftHeld = pad.buttons[14]?.pressed || axX < -0.55;
      const downHeld = pad.buttons[13]?.pressed || axY > 0.55;
      const upHeld = pad.buttons[12]?.pressed || axY < -0.55;

      // Reset scrub acceleration when stick/D-pad released
      if (!leftHeld && !rightHeld) state.scrubHoldCount = 0;

      if (now - state.lastGamepadNav > 155) {
        if (inPlayer) {
          const onSeekBar = document.activeElement === $('seek-bar') || $('player-overlay').classList.contains('idle-hide');
          // LB (-10s), RB (+30s), LT (-60s), RT (+60s), X (Play/Pause)
          if (pad.buttons[4]?.pressed) { cancelScrub(); seekToAbs(absCurrentTime() - 10); pokePlayerUi(); state.lastGamepadNav = now + 120; }
          else if (pad.buttons[5]?.pressed) { cancelScrub(); seekToAbs(absCurrentTime() + 30); pokePlayerUi(); state.lastGamepadNav = now + 120; }
          else if (pad.buttons[6]?.pressed) { cancelScrub(); seekToAbs(absCurrentTime() - 60); pokePlayerUi(); state.lastGamepadNav = now + 180; }
          else if (pad.buttons[7]?.pressed) { cancelScrub(); seekToAbs(absCurrentTime() + 60); pokePlayerUi(); state.lastGamepadNav = now + 180; }
          else if (pad.buttons[2]?.pressed) { $('btn-play-pause').click(); state.lastGamepadNav = now + 180; }
          else if (rightHeld && onSeekBar) { $('seek-bar').focus(); stepScrub(1); state.lastGamepadNav = now; }
          else if (leftHeld && onSeekBar) { $('seek-bar').focus(); stepScrub(-1); state.lastGamepadNav = now; }
          else if (rightHeld) { pokePlayerUi(); moveFocus('right'); state.lastGamepadNav = now; }
          else if (leftHeld) { pokePlayerUi(); moveFocus('left'); state.lastGamepadNav = now; }
          else if (downHeld) { cancelScrub(); pokePlayerUi(); moveFocus('down'); state.lastGamepadNav = now; }
          else if (upHeld) { cancelScrub(); pokePlayerUi(); moveFocus('up'); state.lastGamepadNav = now; }
          else if (pad.buttons[0]?.pressed) {
            if (state.scrubTarget !== null) { commitScrub(); }
            else if ($('player-overlay').classList.contains('idle-hide')) { $('btn-play-pause').click(); }
            else if (document.activeElement === $('seek-bar')) { $('btn-play-pause').click(); }
            else { document.activeElement?.click(); }
            state.lastGamepadNav = now + 180;
          }
          else if (pad.buttons[1]?.pressed) {
            if (state.scrubTarget !== null) cancelScrub();
            else $('btn-close-player').click();
            state.lastGamepadNav = now + 180;
          }
        } else {
          if (rightHeld) { moveFocus('right'); state.lastGamepadNav = now; }
          else if (leftHeld) { moveFocus('left'); state.lastGamepadNav = now; }
          else if (downHeld) { moveFocus('down'); state.lastGamepadNav = now; }
          else if (upHeld) { moveFocus('up'); state.lastGamepadNav = now; }
          else if (pad.buttons[0]?.pressed) { document.activeElement?.click(); state.lastGamepadNav = now + 150; }
          else if (pad.buttons[1]?.pressed) {
            if (!$('btn-back-shows').classList.contains('hidden')) $('btn-back-shows').click();
            state.lastGamepadNav = now + 150;
          }
        }
      }
    }
    requestAnimationFrame(pollGamepad);
  }

  loadAll();
  requestAnimationFrame(pollGamepad);
})();
