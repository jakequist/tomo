// Tomo docs — tiny vanilla helpers. No dependencies, no network.
(function () {
  "use strict";

  // Copy buttons on command blocks. The button copies its code block's text,
  // stripping leading "$ " prompts so the copied command is runnable.
  document.querySelectorAll(".code .copy").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var block = btn.closest(".code");
      if (!block) return;
      var pre = block.querySelector("pre");
      if (!pre) return;
      var text = pre.innerText.replace(/^\s*\$ /gm, "");
      var done = function () {
        btn.textContent = "copied";
        btn.classList.add("done");
        setTimeout(function () {
          btn.textContent = "copy";
          btn.classList.remove("done");
        }, 1600);
      };
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(done, function () {});
      } else {
        var r = document.createRange();
        r.selectNode(pre);
        var s = window.getSelection();
        s.removeAllRanges();
        s.addRange(r);
        try { document.execCommand("copy"); done(); } catch (e) {}
        s.removeAllRanges();
      }
    });
  });

  // Mobile sidebar toggle.
  var menuBtn = document.querySelector(".menu-btn");
  var backdrop = document.querySelector(".backdrop");
  function close() { document.body.classList.remove("nav-open"); }
  if (menuBtn) {
    menuBtn.addEventListener("click", function () {
      document.body.classList.toggle("nav-open");
    });
  }
  if (backdrop) backdrop.addEventListener("click", close);
  document.querySelectorAll(".sidebar a").forEach(function (a) {
    a.addEventListener("click", close);
  });
  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape") close();
  });
})();
