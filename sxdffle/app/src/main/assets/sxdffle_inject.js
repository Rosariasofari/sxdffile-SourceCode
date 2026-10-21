/*
 * SXDffle 页面注入脚本 —— Flash 检测与接管
 * ---------------------------------------------------------------
 * 作用：现代网页里的 <embed>/<object>/<iframe>/<a> 指向的 .swf，
 *       在 WebView 里已经无法执行（Flash 插件已从内核移除）。
 *       本脚本负责把这些内容识别出来，并在其位置叠加一层「可点击遮罩」，
 *       用户点击后由原生层用内置 Flash 引擎（Ruffle 运行时）接管播放。
 *
 * 与原生层的通信：
 *   window.SxdffleBridge.report(...)  仅上报，用于顶部提示条
 *   window.SxdffleBridge.open(...)    请求进入原生播放器
 *
 * 注意：本文件由 assets 读取后注入，不需要在 Kotlin 里做转义。
 */
(function () {
    var SXD = (window.__SXDFFLE__ = window.__SXDFFLE__ || {});
    if (SXD.installed) {
        try { SXD.rescan(); } catch (e) { /* ignore */ }
        return;
    }
    SXD.installed = true;
    SXD.items = [];

    var OVERLAY_CLASS = 'sxdffle-flash-overlay';

    /** 往原生日志里写一行（黑屏时唯一能看到页面内部判断的通道） */
    function dbg(msg) {
        try {
            if (window.SxdffleBridge && window.SxdffleBridge.debug) {
                window.SxdffleBridge.debug(String(msg));
            }
        } catch (e) { /* ignore */ }
    }

    /**
     * 是否处在子框架（frameset 的 frame、内嵌 iframe）里。
     *
     * 联运平台几乎全是「框架套娃」：
     *   669ye: entergame.php(frameset) → game/game.php(iframe) → login_api.php(SWF)
     *   7k7k : index5.php(frameset)     → s1840.sxd.youxi567.com(SWF)
     * 游戏画面在**子框架文档**里，坐标是相对子框架的，不能直接当整页坐标用；
     * 因此子框架上报时左上角固定为 (0,0)，尺寸用框架自身的视口大小。
     */
    var IN_FRAME = false;
    try {
        IN_FRAME = (window.top !== window.self);
    } catch (e) {
        // 跨域访问 window.top 抛异常 → 说明确实在子框架里
        IN_FRAME = true;
    }
    SXD.inFrame = IN_FRAME;

    /** 上报参数：子框架里左上角坐标无意义（置 0），但宽高要用框架自身的 CSS 尺寸 */
    function openEmbedded(url, box) {
        var fv = '';
        try { fv = collectFlashVars(); } catch (e) { }

        // 判断"这个 Flash 是不是整页的主画面"。
        //
        // 联运平台的游戏页经常是：页面居中放一个 1000px 宽的游戏，周围全是底色。
        // 严格按元素矩形摆放，手机上就会看到"网页里再套一个小框"。
        // 宽度占到视口 60% 以上、高度不低于 20% 的，按主画面处理 —— 直接铺满。
        var fill = IN_FRAME;
        if (!fill && box) {
            var vw = window.innerWidth || 1;
            var vh = window.innerHeight || 1;
            fill = box.width >= vw * 0.6 && box.height >= vh * 0.2;
        }
        dbg('openEmbed inFrame=' + IN_FRAME + ' fill=' + fill + ' href=' + location.href +
            ' box=' + (box ? Math.round(box.width) + 'x' + Math.round(box.height) : '-') +
            ' swf=' + url + ' vars=' + (fv ? fv.length : 0));
        try {
            if (fill) {
                // 子框架：从 (0,0) 起，尺寸用框架视口的 CSS 尺寸
                window.SxdffleBridge.openEmbed(
                    url, location.href,
                    0, 0,
                    Math.round(window.innerWidth || 0), Math.round(window.innerHeight || 0),
                    fv || '', 1);
            } else {
                window.SxdffleBridge.openEmbed(
                    url, location.href,
                    Math.round(box.left), Math.round(box.top),
                    Math.round(box.width), Math.round(box.height),
                    fv || '', 0);
            }
        } catch (e) { /* ignore */ }
    }

    /** 把相对地址解析为绝对地址 */
    function absUrl(u) {
        if (!u) return null;
        try { return new URL(u, document.baseURI).href; } catch (e) { return u; }
    }

    /** 判断是否疑似 SWF */
    function looksLikeSwf(url, type) {
        if (!url) return false;
        if (/\.swf(\?|#|$)/i.test(url)) return true;
        if (type && String(type).toLowerCase().indexOf('shockwave') >= 0) return true;
        if (type && String(type).toLowerCase().indexOf('futuresplash') >= 0) return true;
        return false;
    }

    /** 从一个元素上提取 SWF 地址 */
    function extractFromElement(el) {
        var tag = el.tagName ? el.tagName.toUpperCase() : '';
        var type = el.getAttribute ? (el.getAttribute('type') || '') : '';
        var url = null;

        if (tag === 'EMBED') {
            url = el.getAttribute('src') || el.getAttribute('data');
        } else if (tag === 'OBJECT') {
            var params = el.querySelectorAll('param');
            for (var i = 0; i < params.length; i++) {
                var n = (params[i].getAttribute('name') || '').toLowerCase();
                if (n === 'movie' || n === 'src' || n === 'filename') {
                    url = params[i].getAttribute('value');
                    if (url) break;
                }
            }
            if (!url) url = el.getAttribute('data');
        } else if (tag === 'IFRAME' || tag === 'A') {
            url = el.getAttribute('src') || el.getAttribute('href');
        }

        if (!url) return null;
        url = absUrl(url);
        if (!looksLikeSwf(url, type)) return null;
        return url;
    }

    /** 判断元素是否可见且有尺寸 */
    function visibleBox(el) {
        if (!el || !el.getBoundingClientRect) return null;
        var r = el.getBoundingClientRect();
        if (r.width < 8 || r.height < 8) return null;
        var st = window.getComputedStyle ? window.getComputedStyle(el) : null;
        if (st && (st.display === 'none' || st.visibility === 'hidden')) return null;
        return r;
    }

    /**
     * 提取页面上的 flashVars。
     *
     * 这是页游的启动参数载体，《神仙道》有 170+ 项（含 ip / port / hash_code）。
     * 没有它游戏无法完成初始化，所以必须抓出来交给原生引擎。
     *
     * 提取顺序（由可靠到宽松）：
     *   1) 常见全局变量名
     *   2) 遍历 window 找"最像 flashVars"的字符串（形如 k=v&k=v）
     *   3) <embed flashvars> / <object><param name="flashvars">
     *   4) swfobject 注册过的对象
     */
    function looksLikeFlashVars(s) {
        return typeof s === 'string' && s.length > 20 && s.length < 500000 &&
            s.indexOf('&') > 0 && s.indexOf('=') > 0 &&
            /^[A-Za-z_][A-Za-z0-9_]*=/.test(s);
    }

    function collectFlashVars() {
        var best = '';

        // 1) 常见变量名
        var names = ['flashVars', 'flashvars', 'FlashVars', 'flash_vars', 'fv'];
        for (var i = 0; i < names.length; i++) {
            try {
                var v = window[names[i]];
                if (looksLikeFlashVars(v) && v.length > best.length) best = v;
            } catch (e) { }
        }
        if (best) return best;

        // 2) 遍历 window
        try {
            for (var k in window) {
                if (k.length > 40) continue;
                var val;
                try { val = window[k]; } catch (e) { continue; }
                if (looksLikeFlashVars(val) && val.length > best.length) best = val;
            }
        } catch (e) { }
        if (best) return best;

        // 3) DOM 上的 flashvars
        var nodes = document.querySelectorAll('embed, object');
        for (var n = 0; n < nodes.length; n++) {
            var el = nodes[n];
            var fv = el.getAttribute && el.getAttribute('flashvars');
            if (!fv && el.tagName === 'OBJECT') {
                var ps = el.querySelectorAll('param');
                for (var j = 0; j < ps.length; j++) {
                    var nm = (ps[j].getAttribute('name') || '').toLowerCase();
                    if (nm === 'flashvars' || nm === 'flash_vars') {
                        fv = ps[j].getAttribute('value');
                        break;
                    }
                }
            }
            if (looksLikeFlashVars(fv) && fv.length > best.length) best = fv;
        }

        // 4) swfobject 注册表
        try {
            var so = window.swfobject;
            if (so && so.registeredObjects) {
                for (var rid in so.registeredObjects) {
                    var attrs = so.registeredObjects[rid] && so.registeredObjects[rid].attributes;
                    var f2 = attrs && attrs.flashvars;
                    if (looksLikeFlashVars(f2) && f2.length > best.length) best = f2;
                }
            }
        } catch (e) { }

        return best;
    }
    SXD.collectFlashVars = collectFlashVars;

    /** 广告特征：文件名/路径命中即判定为非游戏内容 */
    var AD_PATTERN = /(bcastr|banner|advert|\/ads?\/|gg[._-]|_ad\.|ad_|promo|slide|carousel|logo|nav\.swf|top\.swf|enterbgame)/i;

    /** 判断一个 Flash 元素是否像「游戏主画面」而不是广告 */
    function isLikelyGame(el, url, box) {
        if (box.width < 640 || box.height < 480) return false;

        var u = String(url || '');
        if (AD_PATTERN.test(u)) return false;

        // 静态资源目录下的 SWF 基本都是装饰/广告（bcastr3.swf 就在 /images/ 下）
        if (/\/images?\//i.test(u)) return false;

        // 极端宽高比：横幅、竖条
        var ratio = box.width / box.height;
        if (ratio < 0.4 || ratio > 3.5) return false;

        // 占页面比例太小（广告位常见）
        var vw = window.innerWidth || 1;
        var vh = window.innerHeight || 1;
        if (box.width * box.height < vw * vh * 0.12) return false;

        return true;
    }

    /** 是否页面上面积最大的 Flash（避免一次接管多个） */
    function isLargestFlash(el, box) {
        var all = document.querySelectorAll('embed, object');
        var area = box.width * box.height;
        for (var i = 0; i < all.length; i++) {
            var o = all[i];
            if (o === el) continue;
            var r = o.getBoundingClientRect ? o.getBoundingClientRect() : null;
            if (!r) continue;
            if (r.width * r.height > area * 1.05) return false;
        }
        return true;
    }

    // ---- 让网页能"调用" SWF 注册的回调 ----
    //
    // 页游把 <embed> 当成双向通道：SWF 里 ExternalInterface.addCallback('function_gameW7')
    // 之后，网页用 document.getElementById(id).function_gameW7(...) 反向调用。
    // 我们的引擎跑在网页之外，DOM 上的 <embed> 是个死元素，页面于是打印
    // "function_xxx is not exist"。这里给元素套一层 Proxy：
    //   - 真实属性照常返回（函数绑定到原元素，避免 Illegal invocation）
    //   - 不存在的属性返回一个转发给引擎的函数
    var proxyPatched = false;

    function ensureFlashProxy(el) {
        if (!el || el.__sxdProxy) return el && el.__sxdProxy;
        if (typeof Proxy !== 'function') return el;
        var p = new Proxy(el, {
            get: function (t, prop) {
                if (prop === '__sxdProxy') return p;
                var v;
                try { v = t[prop]; } catch (e) { v = undefined; }
                if (v !== undefined) {
                    return (typeof v === 'function') ? v.bind(t) : v;
                }
                if (typeof prop === 'string' && /^[A-Za-z_$][\w$]*$/.test(prop)) {
                    return function () {
                        var args = Array.prototype.slice.call(arguments);
                        var raw = '';
                        try {
                            raw = window.SxdffleBridge.externalCall(prop, JSON.stringify(args));
                        } catch (e) { return undefined; }
                        // 原生侧返回 JSON 字符串；空串表示没有会话
                        if (typeof raw === 'string' && raw.length > 0) {
                            try { return JSON.parse(raw); } catch (e) { return raw; }
                        }
                        return undefined;
                    };
                }
                return undefined;
            },
            set: function (t, prop, val) { t[prop] = val; return true; }
        });
        el.__sxdProxy = p;
        return p;
    }

    /** 让 getElementById 返回代理对象（swfobject 就是这么取元素的） */
    function patchGetElementById() {
        if (proxyPatched) return;
        proxyPatched = true;
        try {
            var orig = Document.prototype.getElementById;
            Document.prototype.getElementById = function (id) {
                var r = orig.call(this, id);
                return (r && r.__sxdProxy) ? r.__sxdProxy : r;
            };
        } catch (e) { /* ignore */ }
    }

    /** 在元素上方叠加遮罩 */
    function attachOverlay(el, url) {
        if (el.__sxdffleOverlay) return;
        ensureFlashProxy(el);

        var box = visibleBox(el);
        if (!box) return;

        // ---- 自动接管大尺寸 Flash ----
        //
        // 必须严格过滤广告，否则会出大问题：
        // 669ye 首页有个 bcastr3.swf 轮播广告（1000x90 之类），旧规则"≥400x300"
        // 会把它当成游戏自动内嵌，此后 embedView 非空 → 所有页面跳转被拦，
        // 用户连"注册 / 登录"都点不动。
        //
        // 现在的规则（三条同时满足）：
        //   1) 尺寸 ≥ 640x480 —— 页游主画面通常 800x600 以上
        //   2) 宽高比正常（0.4 ~ 3.5）—— 横幅广告会在此被排除
        //   3) URL 不含广告特征词，且不在 /images/ 这类静态资源目录下
        //   4) 只自动接管页面上**面积最大**的那一个
        if (isLikelyGame(el, url, box) && isLargestFlash(el, box)) {
            if (!el.__sxdffleAutoOpened) {
                el.__sxdffleAutoOpened = true;
                SXD.activeEl = el;
                setTimeout(function () { try { SXD.pushRect(); } catch (e) { } }, 400);
                setTimeout(function () { openEmbedded(url, box); }, 1200);
            }
        }

        var overlay = document.createElement('div');
        overlay.className = OVERLAY_CLASS;
        overlay.setAttribute('data-sxdffle-url', url);

        var cs = window.getComputedStyle ? window.getComputedStyle(el) : null;
        overlay.style.cssText = [
            'position:absolute',
            'left:' + (box.left + window.scrollX) + 'px',
            'top:' + (box.top + window.scrollY) + 'px',
            'width:' + box.width + 'px',
            'height:' + box.height + 'px',
            'z-index:2147483000',
            'background:rgba(15,15,15,0.86)',
            'color:#FFD54F',
            'font:13px/1.4 -apple-system,Roboto,sans-serif',
            'display:flex',
            'align-items:center',
            'justify-content:center',
            'flex-direction:column',
            'text-align:center',
            'cursor:pointer',
            'border:1px solid #4CAF50',
            'box-sizing:border-box',
            'padding:6px',
            'z-index:' + (cs && cs.zIndex !== 'auto' ? cs.zIndex : 2147483000)
        ].join(';');

        overlay.innerHTML =
            '<div style="font-size:22px;line-height:1">\u25B6</div>' +
            '<div style="margin-top:4px">SXDffle \u5F15\u64CE\u8FD0\u884C Flash</div>' +
            '<div style="margin-top:2px;color:#9E9E9E;font-size:11px">' +
            Math.round(box.width) + '\u00D7' + Math.round(box.height) + '</div>';

        overlay.addEventListener('click', function (ev) {
            ev.preventDefault();
            ev.stopPropagation();
            openEmbedded(url, box);
        }, true);

        document.body.appendChild(overlay);
        el.__sxdffleOverlay = overlay;

        // 让原来的 Flash 元素不再吃点击事件
        try { el.style.pointerEvents = 'none'; } catch (e) { /* ignore */ }
    }

    /** 扫描整页 */
    SXD.rescan = function () {
        // 子框架通常只是广告位 / 计数器，尺寸太小没必要接管（还会拖慢扫描）
        if (IN_FRAME && ((window.innerWidth || 0) < 320 || (window.innerHeight || 0) < 240)) {
            return 0;
        }

        var found = [];
        var nodes = document.querySelectorAll('embed, object, iframe, a');

        for (var i = 0; i < nodes.length; i++) {
            var el = nodes[i];
            var url = extractFromElement(el);
            if (!url) continue;

            // iframe / 链接同样视为 Flash 入口（常见于页游的「开始游戏」按钮）
            if (el.tagName === 'A' && !looksLikeSwf(url, '')) continue;

            found.push(url);
            if (el.tagName === 'EMBED' || el.tagName === 'OBJECT') {
                attachOverlay(el, url);
            }
        }

        SXD.items = found;
        if (found.length > 0) {
            // 同一处内容会被 MutationObserver 反复扫到，去重后再上报，
            // 否则日志里全是重复行，真出问题时反而看不清。
            var sig = found.length + '|' + found[0];
            if (sig !== SXD.lastReport) {
                SXD.lastReport = sig;
                try {
                    window.SxdffleBridge.report(found[0], location.href, found.length);
                } catch (e) { /* ignore */ }
            }
        }
        return found.length;
    };

    /**
     * 位置同步：内嵌视图是原生 View，覆盖在 WebView 之上，
     * 页面滚动后必须跟着移动，否则会"漂"到别的位置。
     * 用 60ms 节流，避免滚动时高频跨 JNI 调用。
     */
    SXD.activeEl = null;
    var rectTimer = 0;

    function pushRect() {
        var el = SXD.activeEl;
        if (!el || !el.getBoundingClientRect) return;
        var r = el.getBoundingClientRect();
        try {
            window.SxdffleBridge.embedRect(
                Math.round(r.left), Math.round(r.top),
                Math.round(r.width), Math.round(r.height));
        } catch (e) { /* ignore */ }
    }

    function scheduleRect() {
        if (rectTimer) return;
        rectTimer = setTimeout(function () { rectTimer = 0; pushRect(); }, 60);
    }

    SXD.pushRect = pushRect;
    window.addEventListener('scroll', scheduleRect, true);
    window.addEventListener('resize', scheduleRect, true);

    /** 供页面/原生调用的主动打开 */
    SXD.openFirst = function () {
        SXD.rescan();
        if (SXD.items.length > 0) {
            var fv = '';
            try { fv = collectFlashVars(); } catch (e) { }
            try { window.SxdffleBridge.open(SXD.items[0], location.href, 0, 0, fv || ''); } catch (e) { }
            return true;
        }
        return false;
    };

    /**
     * 框架套娃 / 自动提交表单 → 把真正的游戏入口提升为顶层页面。
     *
     * 为什么必须这么做：
     *  1) 7k7k 的 togame.php 返回「自动提交表单」，隐藏字段里带着**带 hash**
     *     的登录地址；表单 POST 到 index5.php 后生成的 frameset 把 hash 丢了，
     *     游戏服判定无效 → 游戏页自己 top.location 弹回专区首页，
     *     用户看到的就是「点了开始游戏，又回到首页」。
     *     在表单自动提交前抓住那个带 hash 的地址、顶层加载它，
     *     游戏服会 Set-Cookie(sxd_user/login_hash_sxd) 并 302 到游戏根页，链路才通。
     *  2) 669ye 的 entergame.php 是 frameset → game.php 里再套 100% iframe
     *     → login_api.php 才是含 SWF 的页面。虽然 document-start 注入已经
     *     覆盖所有 frame，但把入口提到顶层能拿到最干净的 referer 与尺寸。
     *
     * 安全边界（避免在普通网站上乱跳）：
     *   - 只在顶层文档执行
     *   - 只认三种页面：自动提交表单、frameset、几乎只剩一个大 iframe 的页面
     *   - 每个页面只提升一次（__sxdPromoted），原生侧再按 URL 去重
     */
    var ENTRY_HINT = /(login_api|entergame|game_?entry|\/play\/|hash=|^https?:\/\/s\d+\.)/i;

    function promoteEntry() {
        if (IN_FRAME || window.__sxdPromoted) return;
        if (!window.SxdffleBridge || !window.SxdffleBridge.promoteEntry) return;

        // ---- A) 自动提交表单里的游戏地址 ----
        var inputs = document.querySelectorAll('input');
        for (var i = 0; i < inputs.length; i++) {
            var v = inputs[i].value || '';
            if (!/^https?:\/\//i.test(v)) continue;
            if (!ENTRY_HINT.test(v)) continue;
            var f = inputs[i].form;
            // 压掉页面自己的自动提交，避免和我们这次导航抢
            try {
                if (f) f.submit = function () { };
                if (document.body) document.body.removeAttribute('onload');
            } catch (e) { /* ignore */ }
            window.__sxdPromoted = 1;
            try { window.SxdffleBridge.promoteEntry(v, 'form'); } catch (e) { /* ignore */ }
            return;
        }

        // ---- B) frameset / 独占式 iframe ----
        var frames = document.querySelectorAll('frame, iframe');
        if (!frames.length) return;

        var hasFrameset = !!document.querySelector('frameset');
        var vw = window.innerWidth || 1;
        var vh = window.innerHeight || 1;
        var vArea = vw * vh;

        // frameset 页面没有 body，textLen 会算成 0；普通页面则用来判断"框架壳"
        var textLen = 0;
        try {
            textLen = ((document.body && document.body.innerText) || '').replace(/\s/g, '').length;
        } catch (e) { textLen = 999; }

        var best = null, bestScore = -1;
        var skipped = 0;
        for (var j = 0; j < frames.length; j++) {
            var el = frames[j];
            var src = el.getAttribute('src') || '';
            if (!src || src === 'about:blank' || /^javascript:/i.test(src)) { skipped++; continue; }
            var abs = absUrl(src);
            if (!abs || !/^https?:/i.test(abs)) { skipped++; continue; }
            // 同域 iframe 允许提升 —— flash.cn 的登录/进游戏 bridge 就是同域 iframe，
            // 只有把它提到顶层，登录页才能拿到正确的 top 上下文（否则会一直
            // "选完区 → 回登录页"）。
            //
            // 51 那种"提到顶层后又被弹回外层"的乒乓，改由 native 侧的自适应判定处理：
            // 提升后 5 秒内又回到原页面，就把该 host 拉黑，不再提升。

            // <frame> 的 getBoundingClientRect 在部分内核里会抛异常，
            // 一旦抛出去整个提升流程就哑了（黑屏就是这么来的），必须单独兜住。
            var area = 0;
            try {
                var r = el.getBoundingClientRect();
                area = Math.max(r.width, 0) * Math.max(r.height, 0);
            } catch (e) { area = 0; }

            // <frame> 常量不出尺寸，frameset 场景按结构取"排在最后"的那个：
            // 7k7k 是 header / gameNav / gameMain，669ye 是 top_nav / mainFrame ——
            // 主画面都在最后。同分时用 >= 让靠后的覆盖靠前的。
            var score = area + (ENTRY_HINT.test(abs) ? vArea * 10 : 0) + (hasFrameset ? 1 : 0);
            if (score >= bestScore) { bestScore = score; best = { url: abs, area: area }; }
        }

        dbg('promote? frames=' + frames.length + ' 无src=' + skipped +
            ' frameset=' + hasFrameset + ' text=' + textLen +
            ' pick=' + (best ? best.url : 'none'));

        if (!best) return;

        var dominant = best.area >= vArea * 0.7;
        if (!dominant && !hasFrameset) return;
        if (!hasFrameset && textLen >= 200) return;

        window.__sxdPromoted = 1;
        try {
            window.SxdffleBridge.promoteEntry(best.url, hasFrameset ? 'frameset' : 'iframe');
        } catch (e) { /* ignore */ }
    }
    SXD.promoteEntry = promoteEntry;

    /** 当前是不是「框架壳」页面（原生侧用它判断黑屏是不是因为没找到游戏） */
    SXD.hasFrameShell = function () {
        try {
            return !!(document.querySelector('frameset') || document.querySelector('frame'));
        } catch (e) { return false; }
    };

    /** 启动扫描并监听动态插入的 Flash */
    function boot() {
        dbg('boot inFrame=' + IN_FRAME + ' href=' + location.href);
        patchGetElementById();
        try { SXD.rescan(); } catch (e) { }
        try { promoteEntry(); } catch (e) { }

        if (window.MutationObserver) {
            var timer = null;
            var mo = new MutationObserver(function () {
                if (timer) return;
                timer = setTimeout(function () {
                    timer = null;
                    try { SXD.rescan(); } catch (e) { }
                }, 600);
            });
            mo.observe(document.documentElement || document.body, {
                childList: true,
                subtree: true
            });
        }

        // 兜底：延迟再扫几轮，应对纯 JS 延迟渲染的页游
        setTimeout(function () { try { SXD.rescan(); promoteEntry(); } catch (e) { } }, 1500);
        setTimeout(function () { try { SXD.rescan(); promoteEntry(); } catch (e) { } }, 4000);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', boot);
    } else {
        boot();
    }
})();
