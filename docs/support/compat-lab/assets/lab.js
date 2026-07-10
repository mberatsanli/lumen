(function () {
  function markJs(id, text) {
    var node = document.getElementById(id);
    if (!node) {
      return;
    }

    node.className = node.className.replace("js-pending", "js-loaded");
    node.className += " js-loaded";
    node.textContent = text;
  }

  markJs("js-status", "External JS loaded and executed.");
  markJs("html-js-injection", "External JS modified this HTML page.");
  markJs("css-js-injection", "External JS modified this CSS page.");

  document.documentElement.className += " js-enabled";
})();
