// Signal that the queue addon is enabled
window.queueAddonEnabled = true;

document.addEventListener('alpine:init', () => {
    const originalExtensionDetailPage = window.extensionDetailPage;
    if (typeof originalExtensionDetailPage === 'function') {
        window.extensionDetailPage = function (options) {
            const data = originalExtensionDetailPage(options);
            data.queueEnabled = true;
            return data;
        }
    }
});
