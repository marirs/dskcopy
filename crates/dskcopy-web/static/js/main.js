/* main.js — dskcopy landing page interactions */

document.addEventListener('DOMContentLoaded', () => {
    // ── Scroll-reveal ─────────────────────────────────────
    const observer = new IntersectionObserver(
        (entries, obs) => {
            entries.forEach(entry => {
                if (entry.isIntersecting) {
                    entry.target.classList.add('visible');
                    obs.unobserve(entry.target);
                }
            });
        },
        { rootMargin: '0px 0px -60px 0px', threshold: 0.1 }
    );

    document.querySelectorAll('.reveal').forEach(el => observer.observe(el));

    // ── Animated progress bar in hero ─────────────────────
    const bar   = document.getElementById('animBar');
    const label = document.getElementById('animPct');
    if (!bar || !label) return;

    let pct      = 0;
    let target   = 74;
    let running  = false;

    function animateBar() {
        if (running) return;
        running = true;
        pct = 0;
        bar.style.width = '0%';
        label.textContent = '0%';

        const duration = 2200;
        const start    = performance.now();

        function step(now) {
            const elapsed  = now - start;
            const progress = Math.min(elapsed / duration, 1);
            // ease out cubic
            const eased    = 1 - Math.pow(1 - progress, 3);
            pct            = Math.round(eased * target);

            bar.style.width    = pct + '%';
            label.textContent  = pct + '%';

            if (progress < 1) {
                requestAnimationFrame(step);
            } else {
                running = false;
            }
        }
        requestAnimationFrame(step);
    }

    // Trigger bar animation when app-frame enters viewport
    const frameObs = new IntersectionObserver(
        (entries) => {
            entries.forEach(entry => {
                if (entry.isIntersecting) {
                    // small delay so the reveal animation fires first
                    setTimeout(animateBar, 600);
                    frameObs.unobserve(entry.target);
                }
            });
        },
        { threshold: 0.3 }
    );

    const frame = document.querySelector('.app-frame');
    if (frame) frameObs.observe(frame);

    // ── Mobile nav toggle ─────────────────────────────────
    const burger = document.querySelector('.nav-hamburger');
    const navLinks = document.querySelector('.nav-links');

    if (burger && navLinks) {
        burger.addEventListener('click', () => {
            const expanded = burger.getAttribute('aria-expanded') === 'true';
            burger.setAttribute('aria-expanded', !expanded);
            navLinks.classList.toggle('nav-open', !expanded);
        });

        // Close on outside click
        document.addEventListener('click', e => {
            if (!burger.contains(e.target) && !navLinks.contains(e.target)) {
                burger.setAttribute('aria-expanded', 'false');
                navLinks.classList.remove('nav-open');
            }
        });
    }

    // ── Stagger nav-open links ────────────────────────────
    const style = document.createElement('style');
    style.textContent = `
        @media (max-width: 768px) {
            .nav-links {
                display: none;
                position: absolute;
                top: 60px;
                left: 0;
                right: 0;
                background: rgba(24,24,37,.97);
                backdrop-filter: blur(20px);
                -webkit-backdrop-filter: blur(20px);
                border-bottom: 1px solid rgba(69,71,90,.6);
                flex-direction: column;
                gap: 0;
                padding: 8px 0 12px;
            }
            .nav-links.nav-open {
                display: flex;
            }
            .nav-links a:not(.btn) {
                display: block;
                padding: 12px 24px;
                font-size: .95rem;
                color: #a6adc8;
            }
            .nav-links .btn {
                margin: 8px 24px 0;
            }
        }
    `;
    document.head.appendChild(style);
});
