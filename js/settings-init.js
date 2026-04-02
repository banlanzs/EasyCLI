// Page initialization after DOM is ready

document.addEventListener('DOMContentLoaded', async () => {
    try {
        const currentConfig = await getCurrentConfig();
        originalConfig = currentConfig;
        await initializeDebugSwitch();
        await initializePort();
        await initializeProxyUrl();
        await initializeRemoteManagement();
        await initializeAdditionalSettings();
        await initializeAutoStart();
        await loadRuntimeHealthSummary();
        toggleLocalOnlyFields();
        updateServerStatus();
        updateActionButtons();
        initializeAutoUpdateSwitch();

        const currentTabEl = document.querySelector('.tab.active');
        const currentTab = currentTabEl ? currentTabEl.getAttribute('data-tab') : 'basic';
        if (currentTab === 'access-token') {
            await loadAccessTokenKeys();
        } else if (currentTab === 'api') {
            await loadAllApiKeys();
        } else if (currentTab === 'openai') {
            await loadOpenaiProviders();
        }

        // Keep-alive is managed by the backend lifecycle.
    } catch (error) {
        console.error('Error initializing settings:', error);
        showError(i18n.t('msg.failed-load'));
    }
});

