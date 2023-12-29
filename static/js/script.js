// Set initial animation state based on the user's local time
window.addEventListener('load', function() {
    toggleDayNightMode();
    initializeForm();
    initializeQualityControl();
});

// Function to toggle between day and night mode based on time
function toggleDayNightMode() {
    var hour = new Date().getHours();
    var isDayTime = hour > 6 && hour < 18; // Consider day time from 6 AM to 6 PM

    var bodyClass = document.body.classList;
    var toggleIcon = document.getElementById('toggle-icon');
    var backgroundAnimationDay = document.querySelector('.background-animation.day');
    var backgroundAnimationNight = document.querySelector('.background-animation.night');

    if (isDayTime) {
        bodyClass.add('day-mode');
        bodyClass.remove('night-mode');
        toggleIcon.textContent = 'dark_mode'; // Icon for night mode
        backgroundAnimationDay.classList.remove('hidden');
        backgroundAnimationNight.classList.add('hidden');
    } else {
        bodyClass.add('night-mode');
        bodyClass.remove('day-mode');
        toggleIcon.textContent = 'light_mode'; // Icon for day mode
        backgroundAnimationNight.classList.remove('hidden');
        backgroundAnimationDay.classList.add('hidden');
    }
}

function showFeedbackForm() {
    document.getElementById('feedback-modal').classList.remove('hidden');
}

function hideFeedbackForm() {
    document.getElementById('feedback-modal').classList.add('hidden');
}

document.getElementById('feedback-button').addEventListener('click', function() {
    showFeedbackForm();
});


// Toggle between day and night mode on click
document.getElementById('mode-toggle').addEventListener('click', function() {
    var bodyClass = document.body.classList;
    var toggleIcon = document.getElementById('toggle-icon');
    var backgroundAnimationDay = document.querySelector('.background-animation.day');
    var backgroundAnimationNight = document.querySelector('.background-animation.night');
    
    if (bodyClass.contains('day-mode')) {
        bodyClass.replace('day-mode', 'night-mode');
        toggleIcon.textContent = 'light_mode'; // Icon for day mode
        backgroundAnimationDay.classList.add('hidden');
        backgroundAnimationNight.classList.remove('hidden');
    } else {
        bodyClass.replace('night-mode', 'day-mode');
        toggleIcon.textContent = 'dark_mode'; // Icon for night mode
        backgroundAnimationNight.classList.add('hidden');
        backgroundAnimationDay.classList.remove('hidden');
    }
});

document.getElementById('file-input').addEventListener('change', function() {
    if (this.files && this.files.length > 0) {
        document.getElementById('reset-button').classList.remove('hidden');
    }
});

function resetApplication() {
    // Reset the form
    document.getElementById('upload-form').reset();

    // Hide the reset button
    document.getElementById('reset-button').classList.add('hidden');

    // Reset other UI elements (like hiding download links, feedback forms, etc.)
    // For example, if you have a download section:
    document.getElementById('download-section').classList.add('hidden');

    // Reset any other state changes made during the application use
    // ...
}


// Function to initialize form and handle its submission
function initializeForm() {
    document.getElementById('upload-form').addEventListener('submit', function(e) {
        e.preventDefault();

        var action = document.querySelector('input[name="action"]:checked').value;
        var fileInput = document.getElementById('file-input');
        var file = fileInput.files[0];

        // Validate file type
        if (!file.type.match('image.*') && !file.type.match('application/pdf')) {
            alert('Only image files and PDFs are allowed.');
            return;
        }

        var formData = new FormData();
        formData.append('file', file);
        formData.append('action', action);

        if (action === 'compress') {
            var quality = document.getElementById('quality-slider').value;
            formData.append('quality', quality);
        }

        var xhr = new XMLHttpRequest();
        xhr.open('POST', '/upload', true);
        xhr.responseType = 'blob'; // Set the response type to blob for the file download

        // Progress bar setup and event handlers
        var progressBarContainer = document.createElement('div');
        progressBarContainer.className = 'progress-bar-container';
        var progressBar = document.createElement('div');
        progressBar.className = 'progress-bar';
        progressBarContainer.appendChild(progressBar);
        document.body.appendChild(progressBarContainer);

        xhr.upload.onprogress = function(e) {
            if (e.lengthComputable) {
                var percentage = (e.loaded / e.total) * 100;
                progressBar.style.width = percentage + '%';
            }
        };

        xhr.onload = function() {
            if (xhr.status === 200) {
                var filename = xhr.getResponseHeader('Content-Disposition').split('filename=')[1].split(';')[0].replace(/["']/g, "");
                var downloadUrl = URL.createObjectURL(xhr.response);
                var downloadLink = document.getElementById('download-link');
                downloadLink.href = downloadUrl;
                downloadLink.download = filename;
//                downloadLink.download = action === 'pdf_to_image' ? 'image.png' : 'output.pdf'; // Set appropriate file name
                var downloadSection = document.getElementById('download-section');
                downloadSection.classList.remove('hidden');
                downloadSection.classList.add('fade-in');
            } else {
                alert('An error occurred during the upload.');
            }
            progressBarContainer.remove(); // Remove the progress bar after upload is complete
        };

        xhr.send(formData);
    });
}

// Function to initialize quality control visibility and update
function initializeQualityControl() {
    // Update the visibility of the quality control based on the selected action
    document.querySelectorAll('input[name="action"]').forEach(radio => {
        radio.addEventListener('change', function() {
            var qualityControl = document.getElementById('quality-control');
            if (this.value === 'compress') {
                qualityControl.classList.remove('hidden');
            } else {
                qualityControl.classList.add('hidden');
            }
        });
    });

    // Update quality value display when the slider changes
    document.getElementById('quality-slider').addEventListener('input', function() {
        document.getElementById('quality-value').textContent = this.value + '%';
    });
}

// The rest of your existing script.js code, if any
