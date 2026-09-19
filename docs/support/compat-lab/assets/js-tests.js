(function () {
  function text(id, value) {
    var node = document.getElementById(id);
    if (node) {
      node.textContent = value;
    }
  }

  text("js-boot-status", "External JS booted.");

  var created = document.createElement("p");
  created.textContent = "Created by external JavaScript and appended to the DOM.";
  var mount = document.getElementById("dom-created-output");
  if (mount) {
    mount.appendChild(created);
  }

  var count = 0;
  var clickButton = document.getElementById("click-test-button");
  if (clickButton) {
    clickButton.addEventListener("click", function () {
      count += 1;
      text("click-test-output", "Click events observed: " + count);
    });
  }

  var input = document.getElementById("input-test-field");
  if (input) {
    input.addEventListener("input", function () {
      text("input-test-output", "Input event value: " + input.value);
    });
  }

  var timerCount = 0;
  window.setInterval(function () {
    timerCount += 1;
    text("timer-test-output", "Timer ticks: " + timerCount);
  }, 1000);

  var storageButton = document.getElementById("storage-test-button");
  if (storageButton) {
    storageButton.addEventListener("click", function () {
      try {
        window.localStorage.setItem("lumen-compat-lab", "stored");
        text("storage-test-output", window.localStorage.getItem("lumen-compat-lab"));
      } catch (error) {
        text("storage-test-output", "Storage error: " + error.message);
      }
    });
  }

  var fetchButton = document.getElementById("fetch-test-button");
  if (fetchButton) {
    fetchButton.addEventListener("click", function () {
      window.fetch("./assets/sample.json")
        .then(function (response) {
          return response.json();
        })
        .then(function (data) {
          text("fetch-test-output", "Fetched JSON fixture: " + data.name);
        })
        .catch(function (error) {
          text("fetch-test-output", "Fetch error: " + error.message);
        });
    });
  }

  var canvas = document.getElementById("canvas-test");
  if (canvas && canvas.getContext) {
    var ctx = canvas.getContext("2d");
    ctx.fillStyle = "#0f766e";
    ctx.fillRect(0, 0, 220, 100);
    ctx.fillStyle = "#fef3c7";
    ctx.font = "18px sans-serif";
    ctx.fillText("Canvas drew this", 24, 55);
  }
})();
