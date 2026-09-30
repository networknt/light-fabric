// Open rendered Mermaid SVGs outside the documentation column.
(() => {
    const addViewers = () => {
        for (const svg of document.querySelectorAll('.mermaid svg')) {
            const diagram = svg.closest('.mermaid');
            if (diagram.dataset.viewerReady) continue;
            diagram.dataset.viewerReady = 'true';
            const button = document.createElement('button');
            button.type = 'button';
            button.textContent = 'Open diagram in new tab';
            button.style.marginBottom = '0.5rem';
            diagram.before(button);
            button.addEventListener('click', () => {
                const viewer = window.open('about:blank', '_blank');
                if (!viewer) return;
                viewer.opener = null;
                const doc = viewer.document;
                doc.title = 'Diagram — ' + document.title;
                doc.documentElement.lang = document.documentElement.lang || 'en';
                doc.body.style.cssText = 'margin:0;background:#fff;color:#222;font:16px system-ui';
                if (['ayu', 'navy', 'coal'].some(theme => document.documentElement.classList.contains(theme))) {
                    doc.body.style.background = '#1f242b';
                }
                const controls = doc.createElement('nav');
                controls.setAttribute('aria-label', 'Diagram zoom');
                controls.style.cssText = 'position:sticky;top:0;padding:12px;background:#eee;display:flex;gap:12px;align-items:center;z-index:1';
                const canvas = doc.createElement('main');
                canvas.style.padding = '16px';
                const copy = doc.importNode(svg, true);
                copy.style.maxWidth = 'none';
                copy.style.display = 'block';
                canvas.append(copy);
                const box = svg.viewBox.baseVal;
                const width = box.width || svg.getBoundingClientRect().width;
                const height = box.height || svg.getBoundingClientRect().height;
                let scale = 1;
                const label = doc.createElement('span');
                label.setAttribute('aria-live', 'polite');
                const resize = () => {
                    copy.style.width = width * scale + 'px';
                    copy.style.height = height * scale + 'px';
                    copy.setAttribute('width', width * scale);
                    copy.setAttribute('height', height * scale);
                    label.textContent = Math.round(scale * 100) + '%';
                };
                for (const [title, action] of [
                    ['Zoom out', () => { scale = Math.max(0.1, scale / 1.25); }],
                    ['Zoom in', () => { scale = Math.min(10, scale * 1.25); }],
                    ['Actual size', () => { scale = 1; }],
                    ['Fit width', () => { scale = Math.max(0.1, (viewer.innerWidth - 32) / width); }],
                ]) {
                    const control = doc.createElement('button');
                    control.type = 'button';
                    control.textContent = title;
                    control.addEventListener('click', () => { action(); resize(); });
                    controls.append(control);
                }
                controls.append(label);
                doc.body.append(controls, canvas);
                resize();
            });
        }
    };
    // Mermaid renders asynchronously; also handle diagrams inserted afterwards.
    new MutationObserver(addViewers).observe(document.body, { childList: true, subtree: true });
    addViewers();
})();
