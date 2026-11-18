(function () {
  const ICONS = {
    home: '<svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor"><path d="M10 20v-6h4v6h5v-8h3L12 3 2 12h3v8z"/></svg>',
    back: '<svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor"><path d="M20 11H7.83l5.59-5.59L12 4l-8 8 8 8 1.41-1.41L7.83 13H20v-2z"/></svg>',
    forward: '<svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor"><path d="M4 11h12.17l-5.59-5.59L12 4l8 8-8 8-1.41-1.41L16.17 13H4v-2z"/></svg>',
    reload: '<svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor"><path d="M17.65 6.35A7.958 7.958 0 0012 4c-4.42 0-7.99 3.58-7.99 8s3.57 8 7.99 8c3.73 0 6.84-2.55 7.73-6h-2.08c-.82 2.33-3.04 4-5.65 4-3.31 0-6-2.69-6-6s2.69-6 6-6c1.66 0 3.14.69 4.22 1.78L13 11h7V4l-2.35 2.35z"/></svg>',
  };

  function run() {
    if (document.getElementById('jellium-toolbar')) return;

    try {
      const style = document.createElement('style');
      style.textContent = `
        #jellium-toolbar {
          position: fixed;
          top: 16px;
          left: 16px;
          z-index: 2147483647;
          display: flex;
          gap: 5.5px;
        }
        #jellium-toolbar button {
          -webkit-appearance: none;
          appearance: none;
          box-sizing: border-box;
          flex: 0 0 auto;
          width: 40px;
          height: 40px;
          padding: 0;
          margin: 0;
          line-height: 0;
          font-size: 0;
          border-radius: 50%;
          border: 1px solid rgba(255, 255, 255, 0.065);
          background: rgba(255, 255, 255, 0.065);
          backdrop-filter: blur(14px);
          -webkit-backdrop-filter: blur(14px);
          color: rgba(255, 255, 255, 0.92);
          display: flex;
          align-items: center;
          justify-content: center;
          cursor: pointer;
          box-shadow: 0 2px 10px rgba(0, 0, 0, 0.25);
          outline: none;
          transition: background 0.15s ease, color 0.15s ease, transform 0.1s ease;
        }
        #jellium-toolbar button:hover {
          background: #ffffff;
          color: #000000;
        }
        #jellium-toolbar button:active {
          transform: scale(0.94);
        }
        #jellium-toolbar button svg {
          width: 20px;
          height: 20px;
          display: block;
        }
      `;
      document.head.appendChild(style);

      const bar = document.createElement('div');
      bar.id = 'jellium-toolbar';

      const mk = (svg, fn) => {
        const b = document.createElement('button');
        b.innerHTML = svg;
        b.onclick = fn;
        return b;
      };

      bar.appendChild(mk(ICONS.home, () => { window.location.href = '__SERVER_URL__'; }));
      bar.appendChild(mk(ICONS.back, () => history.back()));
      bar.appendChild(mk(ICONS.forward, () => history.forward()));
      bar.appendChild(mk(ICONS.reload, () => location.reload()));

      document.body.appendChild(bar);
    } catch (e) {
      // Fails quietly rather than breaking the host page.
    }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', run);
  } else {
    run();
  }
})();
