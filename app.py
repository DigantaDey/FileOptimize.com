from flask import Flask, request, send_from_directory, after_this_request, render_template, send_file
import os
import threading
import time
from werkzeug.utils import secure_filename
import PIL
from PIL import Image
from PyPDF2 import PdfReader, PdfWriter
from pdf2image import convert_from_path
import shutil
import zipfile
import logging
import mimetypes
from reportlab.pdfgen import canvas
from reportlab.lib.pagesizes import letter
import io
import fitz
#import cv2
import numpy as np
import subprocess

app = Flask(__name__, template_folder='templates', static_folder='static')

# Directories for uploaded and processed files
UPLOAD_FOLDER = '/Users/digantadey/Downloads/fileoptimize/file/uploads'
OUTPUT_FOLDER = '/Users/digantadey/Downloads/fileoptimize/file/processed'

# Ensure directories exist
os.makedirs(UPLOAD_FOLDER, exist_ok=True)
os.makedirs(OUTPUT_FOLDER, exist_ok=True)

app.config['UPLOAD_FOLDER'] = UPLOAD_FOLDER
app.config['OUTPUT_FOLDER'] = OUTPUT_FOLDER

logging.basicConfig(level=logging.INFO)

def delete_file_after_30_mins(filepath):
    time.sleep(1800)  # Wait for 30 minutes
    if os.path.exists(filepath):
        os.remove(filepath)

def compress_image(input_path, output_path, quality):
    try:
        # Open the image
        with Image.open(input_path) as img:
            # Save the image with the specified quality
            img.save(output_path, quality=quality, optimize=True)
    except Exception as e:
        print(f"Error during image compression: {e}")

#def compress_pdf_as_images(input_path, output_path, quality):
#    # Step 1: Convert PDF to Images
#    images = convert_from_path(input_path)
#
#    # Step 2: Compress Images
#    compressed_images = []
#    for i, image in enumerate(images):
#        if image.mode != 'RGB':
#            image = image.convert('RGB')
#        img_byte_arr = io.BytesIO()
#        image.save(img_byte_arr, format='JPEG', quality=quality)
#        img_byte_arr = io.BytesIO(img_byte_arr.getvalue())
#        compressed_images.append(Image.open(img_byte_arr))
#
#    # Step 3: Convert Images Back to PDF
#    compressed_images[0].save(output_path, save_all=True, append_images=compressed_images[1:])
#
#    return output_path


def compress_pdf_as_images(input_path, output_path, quality):
    # Step 1: Convert PDF to Images
    images = convert_from_path(input_path, dpi=150)  # Adjust DPI for size reduction

    # Step 2: Compress Images
    compressed_images = []
    for i, image in enumerate(images):
        # Convert to RGB if not already
        if image.mode != 'RGB':
            image = image.convert('RGB')

        # Resize large images to reduce size further
        max_size = 1280, 1280  # Max width and height
        image.thumbnail(max_size, Image.Resampling.LANCZOS)  # Using LANCZOS for high-quality resampling

        # Save to a byte array with compression
        img_byte_arr = io.BytesIO()
        image.save(img_byte_arr, format='JPEG', quality=quality, optimize=True)
        img_byte_arr = io.BytesIO(img_byte_arr.getvalue())
        compressed_images.append(Image.open(img_byte_arr))

    # Step 3: Convert Images Back to PDF
    if compressed_images:
        compressed_images[0].save(output_path, save_all=True, append_images=compressed_images[1:], quality=quality)

    return output_path


def compress_pdf(input_path, output_path, quality_percentage):
    """
    Compress a PDF file using Ghostscript based on quality percentage.

    Args:
    - input_path: Path to the input PDF file.
    - output_path: Path where the compressed PDF file will be saved.
    - quality_percentage: Integer, percentage of quality (0-100).
    """
    try:
        # Map percentage to Ghostscript quality settings
        if quality_percentage <= 25:
            pdf_setting = '/screen'
        elif quality_percentage <= 50:
            pdf_setting = '/ebook'
        elif quality_percentage <= 75:
            pdf_setting = '/printer'
        else:
            pdf_setting = '/prepress'

        # Construct and run the Ghostscript command
        subprocess.run([
            'gs', '-sDEVICE=pdfwrite', '-dCompatibilityLevel=1.4',
            '-dPDFSETTINGS={}'.format(pdf_setting), '-dNOPAUSE', '-dBATCH', '-dQUIET',
            '-sOutputFile={}'.format(output_path), input_path
        ], check=True)

    except subprocess.CalledProcessError as e:
        print(f"Error during PDF compression: {e}")
    except Exception as e:
        print(f"General error during PDF compression: {e}")

def convert_image_to_pdf(input_path, output_path):
    try:
        # Ensure the output path has a .pdf extension
        if not output_path.endswith('.pdf'):
            output_path = os.path.splitext(output_path)[0] + '.pdf'

        logging.info(f"Converting image to PDF: {input_path} to {output_path}")
        image = Image.open(input_path)
        if image.mode == 'RGBA':
            image = image.convert('RGB')
        image.save(output_path, 'PDF', resolution=100.0)
    except Exception as e:
        logging.error(f"Error in convert_image_to_pdf: {e}")

def convert_pdf_to_image(input_path, output_folder, dpi=200, format='jpeg'):
    try:
        images = convert_from_path(input_path, dpi=dpi, fmt=format, thread_count=4)
        os.makedirs(output_folder, exist_ok=True)
        for i, image in enumerate(images):
            output_path = os.path.join(output_folder, f'page_{i+1}.{format}')
            image.save(output_path, format.upper())
    except Exception as e:
        print(f"An error occurred: {e}")
        return None

@app.route('/')
def index():
    return render_template('index.html')

@app.route('/upload', methods=['POST'])
def upload_file():
    if 'file' not in request.files:
        return 'No file part'
    file = request.files['file']
    if file.filename == '':
        return 'No selected file'
    if file:
        filename = secure_filename(file.filename)
        filepath = os.path.join(app.config['UPLOAD_FOLDER'], filename)
        file.save(filepath)
        
        # Determine the base filename without extension
        base_filename, file_extension = os.path.splitext(filename)

        # Initialize the output filename
        output_filename = f"output_{filename}"
        
        # Change the output filename for image-to-PDF conversion
        action = request.form.get('action')
        if action == 'image_to_pdf':
            output_filename = f"output_{base_filename}.pdf"
        # Set the output file path
        output_filepath = os.path.join(app.config['OUTPUT_FOLDER'], output_filename)

        action = request.form.get('action')
        quality = int(request.form.get('quality', 50))
        if action == 'compress':
            quality = request.form.get('quality', '50')
            logging.info(f"Compression quality: {quality}")
            try:
                quality = int(quality)
            except ValueError:
                logging.error("Invalid quality percentage")
                return "Invalid quality percentage", 400
            # Detect file type and call the appropriate compression function
            mimetype, _ = mimetypes.guess_type(filepath)
            if mimetype and mimetype.startswith('image'):
                compress_image(filepath, output_filepath, quality)
            elif mimetype and mimetype.startswith('application/pdf'):
                if quality<50:
                    compress_pdf_as_images(filepath, output_filepath, quality)
                else:
                    compress_pdf(filepath, output_filepath, quality)
            else:
                return 'Unsupported file type for compression'
        elif action == 'image_to_pdf':
            convert_image_to_pdf(filepath, output_filepath)
        elif action == 'pdf_to_image':
            output_images_folder = os.path.join(app.config['OUTPUT_FOLDER'], f'images_{filename}')
            convert_pdf_to_image(filepath, output_images_folder)
            
            images = os.listdir(output_images_folder)
            if len(images) == 1:
                single_image_path = os.path.join(output_images_folder, images[0])
                threading.Thread(target=delete_file_after_30_mins, args=(single_image_path,)).start()
                return send_file(single_image_path, as_attachment=True)
            else:
                zip_filename = f"{filename}_images.zip"
                zip_filepath = os.path.join(app.config['OUTPUT_FOLDER'], zip_filename)
                with zipfile.ZipFile(zip_filepath, 'w') as zipf:
                    for image_file in images:
                        file_path = os.path.join(output_images_folder, image_file)
                        zipf.write(file_path, arcname=image_file)
                threading.Thread(target=delete_file_after_30_mins, args=(zip_filepath,)).start()
                return send_file(zip_filepath, as_attachment=True)

        threading.Thread(target=delete_file_after_30_mins, args=(filepath,)).start()
        return send_from_directory(app.config['OUTPUT_FOLDER'], output_filename, as_attachment=True)

if __name__ == '__main__':
    app.run(debug=True, host='0.0.0.0', port=5000)