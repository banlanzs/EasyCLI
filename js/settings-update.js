// Manual update check and download for the Settings panel (Basic tab, local-only)

const AUTO_UPDATE_KEY = 'easycli-auto-update';

const autoUpdateSwitch = document.getElementById('auto-update-switch');
const checkUpdateBtn = document.getElementById('check-update-btn');
const updateStatusText = document.getElementById('update-status-text');
const updateProgressBar = document.getElementById('update-progress-bar');
const updateProgressFill = document.getElementById('update-progress-fill');

function setDownloadProgress(pct, show) {
    if (show) {
        updateProgressBar.style.display = 'block';
        updateProgressFill.style.width = pct + '%';
    } else {
        updateProgressBar.style.display = 'none';
        updateProgressFill.style.width = '0%';
    }
}

// Initialize auto-update toggle from localStorage
function initializeAutoUpdateSwitch() {
    const saved = localStorage.getItem(AUTO_UPDATE_KEY);
    autoUpdateSwitch.checked = saved === null ? true : saved === 'true';
}

autoUpdateSwitch.addEventListener('change', () => {
    localStorage.setItem(AUTO_UPDATE_KEY, autoUpdateSwitch.checked ? 'true' : 'false');
});

// Set status text on the update row
function setUpdateStatus(msg, color) {
    updateStatusText.textContent = msg;
    updateStatusText.style.color = color || '';
}

// Manual check-for-updates handler — reuses the same Tauri commands as login flow
checkUpdateBtn.addEventListener('click', async () => {
    if (!window.__TAURI__?.core?.invoke) {
        setUpdateStatus('Tauri environment required', '#dc2626');
        return;
    }

    checkUpdateBtn.disabled = true;
    checkUpdateBtn.textContent = i18n.t('settings.update.checking');
    setUpdateStatus('', '');

    try {
        // Read proxy: prefer config.yaml, fall back to localStorage (set during login)
        const config = await configManager.getConfig().catch(() => ({}));
        const proxyUrl = config['proxy-url'] || localStorage.getItem('proxy-url') || '';

        // Show proxy being used (helps debug)
        if (proxyUrl) {
            setUpdateStatus(`proxy: ${proxyUrl}`, '#6b7280');
        }

        const result = await window.__TAURI__.core.invoke('check_version_and_download', { proxyUrl });

        if (!result.success) {
            setUpdateStatus(i18n.t('settings.update.failed') + ' ' + (result.error || ''), '#dc2626');
            return;
        }

        if (!result.needsUpdate) {
            // Already latest
            setUpdateStatus(
                `${i18n.t('settings.update.up-to-date')} (${result.version})`,
                '#10b981'
            );
            return;
        }

        // Update available — ask user
        const current = result.version || i18n.t('settings.update.no-local');
        const latest = result.latestVersion || '';
        const msg = i18n.t('settings.update.confirm')
            .replace('{current}', current)
            .replace('{latest}', latest);

        if (!confirm(msg)) {
            setUpdateStatus(
                `${i18n.t('settings.update.current-version')} ${current} / ${i18n.t('settings.update.latest-version')} ${latest}`,
                '#6b7280'
            );
            return;
        }

        // User confirmed — download
        checkUpdateBtn.textContent = i18n.t('settings.update.downloading');
        setUpdateStatus(i18n.t('settings.update.downloading'), '#6b7280');

        // Listen for download progress events
        let unlistenProgress = null;
        let unlistenStatus = null;
        try {
            if (window.__TAURI__?.event?.listen) {
                unlistenProgress = await window.__TAURI__.event.listen('download-progress', (event) => {
                    const { progress, downloaded, total } = event.payload;
                    const pct = progress > 0 ? Math.round(progress) : 0;
                    const mbDone = (downloaded / 1024 / 1024).toFixed(1);
                    const mbTotal = total > 0 ? `/${(total / 1024 / 1024).toFixed(1)} MB` : ' MB';
                    setUpdateStatus(i18n.t('settings.update.downloading') + ` ${pct}% (${mbDone}${mbTotal})`, '#6b7280');
                    setDownloadProgress(pct, true);
                });
                unlistenStatus = await window.__TAURI__.event.listen('download-status', (event) => {
                    const { status } = event.payload;
                    if (status === 'starting') {
                        setUpdateStatus(i18n.t('settings.update.downloading') + ' 0%', '#6b7280');
                        setDownloadProgress(0, true);
                    }
                });
            }
        } catch (_) {}

        const dlResult = await window.__TAURI__.core.invoke('download_cliproxyapi', { proxyUrl });

        // Cleanup listeners
        if (unlistenProgress) unlistenProgress();
        if (unlistenStatus) unlistenStatus();

        if (dlResult.success) {
            setDownloadProgress(100, true);
            setTimeout(() => setDownloadProgress(0, false), 800);
            setUpdateStatus(
                i18n.t('settings.update.success').replace('{version}', dlResult.version || latest),
                '#10b981'
            );
            // Restart CLIProxyAPI so it picks up the new binary
            showSuccessMessage(i18n.t('msg.port-restart'));
            await window.__TAURI__.core.invoke('restart_cliproxyapi');
        } else {
            setDownloadProgress(0, false);
            setUpdateStatus(i18n.t('settings.update.failed') + ' ' + (dlResult.error || ''), '#dc2626');
        }
    } catch (err) {
        const msg = (err && (typeof err === 'string' ? err : err.message)) || 'Unknown error';
        setUpdateStatus(i18n.t('settings.update.failed') + ' ' + msg, '#dc2626');
    } finally {
        setDownloadProgress(0, false);
        checkUpdateBtn.disabled = false;
        checkUpdateBtn.textContent = i18n.t('settings.update.check-btn');
    }
});

// Initialize on load
initializeAutoUpdateSwitch();
